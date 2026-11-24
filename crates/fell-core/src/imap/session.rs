//! Blocking IMAP session. Read-only commands only: CAPABILITY, STARTTLS,
//! LOGIN, LIST, EXAMINE, FETCH, UID FETCH, UID SEARCH, ENABLE, IDLE, LOGOUT.
//! The write-back outbox moves mail on a separate [`writes::WriteSession`].

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

mod writes;
pub(super) use writes::{FlagReply, Placed, WriteError, WriteSession};

const TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct Listed {
    pub raw: String,
    pub decoded: String,
    pub delimiter: Option<char>,
    /// LIST attributes, including RFC 6154 SPECIAL-USE marks like `\Sent`.
    pub attributes: Vec<String>,
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
    /// A body reply can omit FLAGS. Only a present list is keyword evidence.
    pub flags_present: bool,
    pub modseq: Option<u64>,
    pub body: Vec<u8>,
    /// The body was larger than the cap. The UID is kept so the checkpoint
    /// can move past it; the bytes were not stored.
    pub skipped: bool,
    /// `BODY[]` was not a literal, quoted string, or NIL, so the bytes were
    /// never read. The checkpoint must stay below this UID.
    pub body_missing: bool,
    /// `RFC822.SIZE`, when the reply had it.
    pub size: Option<u32>,
    pub internal_date: Option<i64>,
}

/// One reply to [`Session::uid_fetch_part`].
#[derive(Debug)]
pub(super) struct PartWindow {
    /// The `<origin>` the server put after `BODY[<section>]`. `None` when it
    /// sent none: it ignored the window and this is the whole part.
    pub origin: Option<usize>,
    pub bytes: Vec<u8>,
}

/// Why [`Session::idle`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdleEnd {
    /// The server reported new, removed, or changed mail.
    Changed,
    /// `max` passed with no news.
    Quiet,
    /// The caller asked to stop.
    Stopped,
}

/// How often IDLE looks up from the socket to see whether to stop.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// How a streaming UID FETCH ended. `Truncated` means the untagged cap was hit;
/// the rest of that command was discarded and the connection is still usable.
pub(super) enum FetchStop {
    Done,
    Refused,
    Truncated { min_skipped_uid: Option<u32> },
}

/// What a streaming UID FETCH asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FetchItems {
    /// `UID FLAGS`: enough to update a message already stored.
    Flags,
    /// The whole message and its server date, for mail not stored yet.
    Bodies,
    /// Enough for a list row and threading, before the body is fetched.
    Headers,
    /// Identity headers, to link copies without their bodies.
    MessageId { bridge: bool },
    /// The original Reply-To header only, for legacy rows.
    ReplyTo,
    /// The server arrival date for legacy rows with no usable date.
    InternalDate,
}

const REPLY_TO_SECTION: &str = "HEADER.FIELDS (REPLY-TO)";
fn identity_section(bridge: bool) -> &'static str {
    if bridge {
        "HEADER.FIELDS (MESSAGE-ID X-PM-INTERNAL-ID)"
    } else {
        "HEADER.FIELDS (MESSAGE-ID)"
    }
}

/// A UID and the header bytes fetched for it.
pub(super) type HeaderRow = (u32, Vec<u8>);

pub(super) struct Session {
    io: Option<Io>,
    tag: u32,
    caps: Vec<String>,
    /// Server advertised CONDSTORE after login, so EXAMINE sends that modifier.
    condstore: bool,
    /// The selected mailbox reported HIGHESTMODSEQ, so FETCH can ask for MODSEQ.
    mailbox_modseq: bool,
    /// `ENABLE QRESYNC` succeeded, so a CHANGEDSINCE fetch can ask for VANISHED.
    qresync: bool,
    /// The server announced an expunge since the last EXAMINE, so message
    /// sequence numbers no longer match the EXISTS count EXAMINE reported.
    expunged: bool,
    /// UID ranges from `VANISHED` replies since the caller last took them.
    vanished: Vec<(u32, u32)>,
    /// A `VANISHED` set used `*` or could not be read, so `vanished` does not
    /// say which UIDs left.
    vanished_unknown: bool,
    host: String,
    pending_buf: Vec<u8>,
}

enum Io {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, Prefixed<TcpStream>>>),
}

impl Session {
    pub(super) fn open(config: &ImapConfig) -> Result<Self, Error> {
        Self::open_with(config, |_| Ok(()))
    }

    pub(super) fn open_with(
        config: &ImapConfig,
        connected: impl FnOnce(&TcpStream) -> Result<(), Error>,
    ) -> Result<Self, Error> {
        if config.accept_invalid_certs && !is_loopback_host(&config.host) {
            return Err(imap_err(
                "accept_invalid_certs is only allowed for a loopback host",
            ));
        }
        let stream = connect_tcp(config)?;
        connected(&stream)?;
        let mut session = match config.security {
            ImapSecurity::Tls => Session {
                io: Some(Io::Tls(Box::new(wrap_tls(stream, Vec::new(), config)?))),
                tag: 0,
                caps: Vec::new(),
                condstore: false,
                mailbox_modseq: false,
                qresync: false,
                expunged: false,
                vanished: Vec::new(),
                vanished_unknown: false,
                host: config.host.clone(),
                pending_buf: Vec::new(),
            },
            ImapSecurity::Plain | ImapSecurity::StartTls => Session {
                io: Some(Io::Plain(stream)),
                tag: 0,
                caps: Vec::new(),
                condstore: false,
                mailbox_modseq: false,
                qresync: false,
                expunged: false,
                vanished: Vec::new(),
                vanished_unknown: false,
                host: config.host.clone(),
                pending_buf: Vec::new(),
            },
        };
        session.read_greeting()?;
        if config.security == ImapSecurity::StartTls {
            session
                .command("STARTTLS", "STARTTLS")
                .map_err(|err| Error::Tls(err.to_string()))?;
            session.upgrade(config)?;
        }
        let logged_in = session.login(&config.username, &config.password)?;
        // RFC 5161: ENABLE is only valid after authentication. A pre-login
        // CAPABILITY is not repeated here unless LOGIN omitted one.
        if let Some(caps) = capability_from_ok(&logged_in.ok) {
            session.caps = caps;
        } else {
            session.read_capabilities()?;
        }
        if session.has_cap("CONDSTORE") {
            let _ = session.command("ENABLE", "ENABLE CONDSTORE");
            session.condstore = true;
        }
        // RFC 7162: QRESYNC implies CONDSTORE. Only a server that confirms it
        // is asked for VANISHED; anything else falls back to a UID listing.
        if session.has_cap("QRESYNC")
            && let Ok(reply) = session.command("ENABLE", "ENABLE QRESYNC")
            && reply
                .untagged
                .iter()
                .any(|line| enabled_lists(&line.text, "QRESYNC"))
        {
            session.qresync = true;
            session.condstore = true;
        }
        Ok(session)
    }

    /// Selectable mailboxes with their LIST attributes.
    ///
    /// A server that advertises SPECIAL-USE (RFC 6154) is asked to mark
    /// Sent, Trash, and the rest with `RETURN (SPECIAL-USE)`. Others may
    /// still mark them on a plain LIST, so attributes are kept either way.
    /// A server that advertises it but refuses the option gets a plain LIST.
    pub(super) fn list_selectable(&mut self) -> Result<Vec<Listed>, Error> {
        const PLAIN: &str = "LIST \"\" \"*\"";
        let lines = if self.has_cap("SPECIAL-USE") {
            match self.command("LIST", "LIST \"\" \"*\" RETURN (SPECIAL-USE)") {
                Ok(reply) => reply.untagged,
                Err(Error::Backend(message)) if message.contains("LIST failed:") => {
                    self.command("LIST", PLAIN)?.untagged
                }
                Err(err) => return Err(err),
            }
        } else {
            self.command("LIST", PLAIN)?.untagged
        };
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
                delimiter: listed.delimiter,
                attributes: listed.attributes,
            });
        }
        Ok(out)
    }

    /// `Ok(None)` when the server refuses this mailbox (`NO`/`BAD`). The
    /// connection is still usable. Other errors abort the sync.
    pub(super) fn examine(&mut self, mailbox: &str) -> Result<Option<Examined>, Error> {
        // CR, LF, or NUL cannot be quoted. Skip the mailbox instead of failing
        // the rest of the account.
        let Ok(name) = quote_imap(mailbox) else {
            return Ok(None);
        };
        let examine = if self.condstore {
            format!("EXAMINE {name} (CONDSTORE)")
        } else {
            format!("EXAMINE {name}")
        };
        let lines = match self.command("EXAMINE", &examine) {
            Ok(reply) => reply.untagged,
            Err(Error::Backend(message)) if message.contains("EXAMINE failed:") => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
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
        self.mailbox_modseq = highest_modseq.is_some();
        self.expunged = false;
        self.vanished.clear();
        self.vanished_unknown = false;
        Ok(Some(Examined {
            uidvalidity,
            uidnext,
            exists,
            highest_modseq,
        }))
    }

    pub(super) fn tcp(&self) -> Result<&TcpStream, Error> {
        match self.io.as_ref() {
            Some(Io::Plain(tcp)) => Ok(tcp),
            Some(Io::Tls(stream)) => Ok(&stream.sock.inner),
            None => Err(imap_err("connection closed")),
        }
    }

    pub(super) fn uid_structures(
        &mut self,
        set: &str,
    ) -> Result<Vec<(u32, super::BodyStructure)>, Error> {
        let reply = self.command(
            "UID FETCH BODYSTRUCTURE",
            &format!("UID FETCH {set} (UID BODYSTRUCTURE)"),
        )?;
        let mut found = Vec::new();
        for line in reply.untagged {
            let upper = line.text.to_ascii_uppercase();
            let Some(value) = fetch_item(&line.text, &upper, "BODYSTRUCTURE", '(') else {
                continue;
            };
            let Some(uid) = fetch_number(&line.text, &upper, "UID") else {
                continue;
            };
            let before = line.text.len() - value.len() - 1;
            let literals = line
                .literals
                .iter()
                .skip(literals_before(&line.text[..before]));
            if let Some(structure) = super::structure::parse(
                &line.text[before..],
                literals.map(|literal| match literal {
                    LiteralBytes::Kept(bytes) => Some(bytes.as_slice()),
                    LiteralBytes::Discarded => None,
                }),
            ) {
                found.push((uid, structure));
            }
        }
        Ok(found)
    }

    /// Flags only. Used to learn unread state before the first modseq is stored.
    pub(super) fn uid_flags(&mut self, range: &str) -> Result<Vec<Fetched>, Error> {
        let revision = if self.mailbox_modseq { " MODSEQ" } else { "" };
        let lines = self
            .command(
                "UID FETCH",
                &format!("UID FETCH {range} (UID FLAGS{revision})"),
            )?
            .untagged;
        Ok(lines
            .iter()
            .filter_map(|line| parse_flags(&line.text))
            .collect())
    }

    /// Deliver each FETCH as it arrives. Bodies are not kept after `on_fetched`
    /// returns. Past the untagged cap, further replies are read and dropped so
    /// the next command stays in sync.
    ///
    /// With [`FetchItems::Flags`] no body is asked for, and each [`Fetched`]
    /// has an empty body that is neither skipped nor missing.
    pub(super) fn uid_fetch_each(
        &mut self,
        range: &str,
        changed_since: Option<u64>,
        items: FetchItems,
        mut on_fetched: impl FnMut(Fetched) -> Result<(), Error>,
    ) -> Result<FetchStop, Error> {
        let identity;
        let body = match items {
            FetchItems::Flags => "",
            FetchItems::Bodies => " INTERNALDATE BODY.PEEK[]",
            FetchItems::Headers => " RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER]",
            FetchItems::MessageId { bridge } => {
                identity = format!(" BODY.PEEK[{}]", identity_section(bridge));
                &identity
            }
            FetchItems::ReplyTo => " BODY.PEEK[HEADER.FIELDS (REPLY-TO)]",
            FetchItems::InternalDate => " INTERNALDATE",
        };
        // CHANGEDSINCE already requests MODSEQ implicitly. Other FETCHes
        // need to request it to retain a baseline for later conditional STORE.
        let revision = if self.mailbox_modseq && changed_since.is_none() {
            " MODSEQ"
        } else {
            ""
        };
        let wanted = if items == FetchItems::InternalDate {
            "(UID INTERNALDATE)".into()
        } else {
            format!("(UID FLAGS{revision}{body})")
        };
        let cmd = match changed_since {
            Some(modseq) if self.qresync => {
                format!("UID FETCH {range} {wanted} (CHANGEDSINCE {modseq} VANISHED)")
            }
            Some(modseq) => format!("UID FETCH {range} {wanted} (CHANGEDSINCE {modseq})"),
            None => format!("UID FETCH {range} {wanted}"),
        };
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.write_all(format!("{tag} {cmd}\r\n").as_bytes())?;
        let mut seen = 0usize;
        let mut discarding = false;
        let mut min_skipped = None;
        loop {
            let (text, literals) = self.read_logical()?;
            if text_starts_with_tag(&text, &tag) {
                let rest = text[tag.len()..].trim_start();
                let upper = rest.to_ascii_uppercase();
                if !upper.starts_with("OK") {
                    return Ok(FetchStop::Refused);
                }
                if discarding {
                    return Ok(FetchStop::Truncated {
                        min_skipped_uid: min_skipped,
                    });
                }
                return Ok(FetchStop::Done);
            }
            self.note_untagged(&text);
            seen += 1;
            if discarding || seen > untagged_limit() {
                discarding = true;
                if let Some(uid) = uid_in_fetch_text(&text) {
                    min_skipped = Some(min_skipped.map_or(uid, |current| current.min(uid)));
                }
                continue;
            }
            let fetched = match items {
                FetchItems::Bodies | FetchItems::Headers | FetchItems::InternalDate => {
                    parse_fetch(&text, literals)
                }
                FetchItems::MessageId { bridge } => {
                    parse_fetch_with_section(&text, literals, Some(identity_section(bridge)))
                }
                FetchItems::ReplyTo => {
                    parse_fetch_with_section(&text, literals, Some(REPLY_TO_SECTION))
                }
                FetchItems::Flags => parse_flags(&text),
            };
            if let Some(fetched) = fetched {
                on_fetched(fetched)?;
            }
        }
    }

    /// UIDs of messages `first..=last` by sequence number, in order.
    /// `Ok(None)` when the server refuses the search.
    pub(super) fn uid_search_seq(
        &mut self,
        first: u32,
        last: u32,
    ) -> Result<Option<Vec<u32>>, Error> {
        self.uid_search(&format!("{first}:{last}"))
    }

    /// UIDs present in a UID range, which can contain gaps.
    pub(super) fn uid_search_range(
        &mut self,
        low: u32,
        high: u32,
    ) -> Result<Option<Vec<u32>>, Error> {
        self.uid_search(&format!("UID {low}:{high}"))
    }

    fn uid_search(&mut self, criteria: &str) -> Result<Option<Vec<u32>>, Error> {
        let lines = match self.command("UID SEARCH", &format!("UID SEARCH {criteria}")) {
            Ok(reply) => reply.untagged,
            Err(Error::Backend(message)) if message.contains("UID SEARCH failed:") => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        Ok(search_uids(&lines, usize::MAX))
    }

    /// UID and `Message-ID` header of messages `first..=last` by sequence
    /// number. Headers only, with `BODY.PEEK`, so nothing is marked read.
    /// `Ok(None)` when the server refuses the fetch.
    pub(super) fn fetch_message_ids(
        &mut self,
        first: u32,
        last: u32,
        bridge: bool,
    ) -> Result<Option<Vec<HeaderRow>>, Error> {
        let cmd = format!(
            "FETCH {first}:{last} (UID BODY.PEEK[{}])",
            identity_section(bridge)
        );
        let lines = match self.command("FETCH", &cmd) {
            Ok(reply) => reply.untagged,
            Err(Error::Backend(message)) if message.contains("FETCH failed:") => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        let mut out = Vec::new();
        for line in lines {
            if let Some(fetched) = parse_fetch(&line.text, line.literals) {
                out.push((fetched.uid, fetched.body));
            }
        }
        out.sort_by_key(|(uid, _)| *uid);
        Ok(Some(out))
    }

    /// One window of one MIME part, still transfer-encoded:
    /// `UID FETCH <uid> (UID BODY.PEEK[<section>]<offset.len>)`. `BODY.PEEK`
    /// leaves the message unread. `Ok(None)` when the reply has no body for
    /// `uid` (the server no longer has it).
    ///
    /// The reply is read the way [`Session::uid_fetch_each`] reads bodies:
    /// one literal up to the literal cap, with no cap on the reply as a
    /// whole, since a server that ignores the window sends the whole part
    /// at once. [`PartWindow::origin`] tells the caller which it got.
    pub(super) fn uid_fetch_part(
        &mut self,
        uid: u32,
        section: &str,
        offset: usize,
        len: usize,
        draft_budget: Option<usize>,
    ) -> Result<Option<PartWindow>, Error> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        let size = if draft_budget.is_some() && offset == 0 {
            " RFC822.SIZE"
        } else {
            ""
        };
        let cmd = format!("UID FETCH {uid} (UID{size} BODY.PEEK[{section}]<{offset}.{len}>)");
        self.write_all(format!("{tag} {cmd}\r\n").as_bytes())?;
        let mut found: Option<Result<PartWindow, Error>> = None;
        let mut seen = 0usize;
        loop {
            let (text, literals) = self.read_logical_with_draft_budget(draft_budget)?;
            if draft_budget.is_some() && draft_size_exceeded(text.as_bytes()) {
                self.disconnect();
                return Err(Error::DraftMessageLimit);
            }
            if text_starts_with_tag(&text, &tag) {
                let rest = text[tag.len()..].trim_start();
                if !rest.to_ascii_uppercase().starts_with("OK") {
                    return Err(imap_err(format!("UID FETCH failed: {rest}")));
                }
                return found.transpose();
            }
            self.note_untagged(&text);
            seen += 1;
            if found.is_some() || seen > untagged_limit() {
                continue;
            }
            let origin = partial_origin(&text);
            let Some(fetched) = parse_fetch(&text, literals) else {
                continue;
            };
            if fetched.uid != uid || fetched.body_missing {
                continue;
            }
            found = Some(if fetched.skipped {
                Err(imap_err(format!(
                    "part {section} of UID {uid} is larger than the {} byte literal limit",
                    literal_limit()
                )))
            } else {
                Ok(PartWindow {
                    origin,
                    bytes: fetched.body,
                })
            });
        }
    }

    /// `ENABLE QRESYNC` succeeded on this session.
    pub(super) fn qresync(&self) -> bool {
        self.qresync
    }

    /// The server announced an expunge since the last EXAMINE.
    pub(super) fn saw_expunge(&self) -> bool {
        self.expunged
    }

    /// UID ranges reported by `VANISHED` since the last call. `None` when a
    /// reply could not be trusted to name only UIDs that left (a `*`, which
    /// stands for whatever UID is highest now, or a set that did not parse).
    pub(super) fn take_vanished(&mut self) -> Option<Vec<(u32, u32)>> {
        let ranges = std::mem::take(&mut self.vanished);
        if std::mem::take(&mut self.vanished_unknown) {
            None
        } else {
            Some(ranges)
        }
    }

    /// Watch untagged replies for expunges. Sequence numbers shift after one,
    /// and with QRESYNC the `VANISHED` reply names the UIDs that left.
    fn note_untagged(&mut self, text: &str) {
        let Some(rest) = text.trim().strip_prefix('*') else {
            return;
        };
        let rest = rest.trim_start();
        if let Some(set) = strip_prefix_ignore_ascii_case(rest, "VANISHED ") {
            let set = set.trim_start();
            let (earlier, set) = match strip_prefix_ignore_ascii_case(set, "(EARLIER)") {
                Some(after) => (true, after.trim_start()),
                None => (false, set),
            };
            if !earlier {
                self.expunged = true;
            }
            match parse_uid_set(set) {
                Some(ranges) => self.vanished.extend(ranges),
                None => self.vanished_unknown = true,
            }
            return;
        }
        let mut words = rest.split_whitespace();
        if words
            .next()
            .is_some_and(|word| word.bytes().all(|byte| byte.is_ascii_digit()))
            && words
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("EXPUNGE"))
        {
            self.expunged = true;
        }
    }

    pub(super) fn has_idle(&self) -> bool {
        self.has_cap("IDLE")
    }

    /// RFC 2177 IDLE on the mailbox already examined. Returns when the server
    /// sends `EXISTS`, `EXPUNGE`, `RECENT`, or `FETCH`, when `max` passes, or
    /// when `stop` returns true (checked every [`IDLE_POLL`]). Always ends
    /// IDLE with `DONE` and reads the tagged reply, so the session stays
    /// usable. Read-only: IDLE changes nothing on the server.
    pub(super) fn idle(
        &mut self,
        max: Duration,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<IdleEnd, Error> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.write_all(format!("{tag} IDLE\r\n").as_bytes())?;
        // Untagged news can come before the continuation. It still counts.
        let mut news = false;
        loop {
            let (line, _) = self.read_logical()?;
            if line.starts_with('+') {
                break;
            }
            if text_starts_with_tag(&line, &tag) {
                return Err(imap_err(format!(
                    "IDLE refused: {}",
                    line[tag.len()..].trim()
                )));
            }
            if is_bye(line.as_bytes()) {
                self.disconnect();
                return Err(imap_err(format!("{} closed the connection", self.host)));
            }
            news |= idle_news(line.as_bytes());
        }
        self.set_read_timeout(IDLE_POLL)?;
        let started = std::time::Instant::now();
        let end = loop {
            if news {
                break Ok(IdleEnd::Changed);
            }
            match self.idle_line() {
                Ok(Some(line)) => {
                    if text_starts_with_tag(&String::from_utf8_lossy(&line), &tag) {
                        // The server ended IDLE itself; there is nothing to DONE.
                        let _ = self.set_read_timeout(TIMEOUT);
                        let text = String::from_utf8_lossy(&line).into_owned();
                        return Err(imap_err(format!(
                            "IDLE ended: {}",
                            text[tag.len()..].trim()
                        )));
                    }
                    if is_bye(&line) {
                        self.disconnect();
                        return Err(imap_err(format!("{} closed the connection", self.host)));
                    }
                    // A literal follows: news, and the reply read after DONE
                    // consumes the literal instead of parsing it as lines.
                    news = idle_news(&line)
                        || !matches!(literal_marker(&line), LiteralMarker::NotLiteral);
                    if news {
                        continue;
                    }
                }
                Ok(None) => {}
                Err(err) => break Err(err),
            }
            if stop() {
                break Ok(IdleEnd::Stopped);
            }
            if started.elapsed() >= max {
                break Ok(IdleEnd::Quiet);
            }
        };
        let restored = self.set_read_timeout(TIMEOUT);
        let end = end?;
        restored?;
        self.write_all(b"DONE\r\n")?;
        self.read_reply(&tag, "IDLE")?;
        Ok(end)
    }

    /// One line, or `None` when the read timed out. Bytes of a partial line
    /// stay buffered for the next call.
    fn idle_line(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let mut buf = self.take_buf();
        loop {
            if let Some(pos) = buf.iter().position(|byte| *byte == b'\n') {
                let mut line = buf.drain(..=pos).collect::<Vec<_>>();
                self.restore_buf(buf);
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(line));
            }
            if buf.len() > line_limit() {
                return Err(imap_err(format!(
                    "response line from {} exceeds the {} byte limit",
                    self.host,
                    line_limit()
                )));
            }
            let mut tmp = [0u8; 4096];
            let Some(io) = self.io.as_mut() else {
                return Err(imap_err("connection closed"));
            };
            match io.read(&mut tmp) {
                Ok(0) => {
                    return Err(imap_err(format!("connection to {} closed", self.host)));
                }
                Ok(count) => buf.extend_from_slice(&tmp[..count]),
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    self.restore_buf(buf);
                    return Ok(None);
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => {
                    return Err(imap_err(format!("read from {} failed: {err}", self.host)));
                }
            }
        }
    }

    fn set_read_timeout(&self, timeout: Duration) -> Result<(), Error> {
        let tcp = match self.io.as_ref() {
            Some(Io::Plain(tcp)) => tcp,
            Some(Io::Tls(stream)) => &stream.sock.inner,
            None => return Err(imap_err("connection closed")),
        };
        tcp.set_read_timeout(Some(timeout))
            .map_err(|err| imap_err(format!("read timeout on {}: {err}", self.host)))
    }

    pub(super) fn logout(&mut self) {
        let _ = self.command("LOGOUT", "LOGOUT");
    }

    fn upgrade(&mut self, config: &ImapConfig) -> Result<(), Error> {
        let pending = std::mem::take(&mut self.pending_buf);
        let Some(Io::Plain(tcp)) = self.io.take() else {
            return Err(imap_err("STARTTLS requires a cleartext session"));
        };
        self.io = Some(Io::Tls(Box::new(wrap_tls(tcp, pending, config)?)));
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
        let lines = self.command("CAPABILITY", "CAPABILITY")?.untagged;
        self.caps = caps_from(&lines);
        Ok(())
    }

    fn has_cap(&self, name: &str) -> bool {
        self.caps.iter().any(|cap| cap.eq_ignore_ascii_case(name))
    }

    /// Username and password are IMAP literals, so octets above 7-bit are legal.
    /// When the server advertises `UTF8=ACCEPT`, enable it before LOGIN if a
    /// credential is not ASCII. Some servers reject the password until then.
    fn login(&mut self, username: &str, password: &str) -> Result<Reply, Error> {
        let non_ascii =
            username.bytes().any(|byte| byte > 127) || password.bytes().any(|byte| byte > 127);
        if non_ascii && !self.has_cap("UTF8=ACCEPT") {
            let _ = self.read_capabilities();
        }
        if non_ascii && self.has_cap("UTF8=ACCEPT") {
            let _ = self.command("ENABLE", "ENABLE UTF8=ACCEPT");
        }
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.write_all(format!("{tag} LOGIN ").as_bytes())?;
        self.write_literal(username.as_bytes())?;
        self.write_all(b" ")?;
        self.write_literal(password.as_bytes())?;
        self.write_all(b"\r\n")?;
        self.read_reply(&tag, "LOGIN")
    }

    fn write_literal(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.write_all(format!("{{{}}}\r\n", bytes.len()).as_bytes())?;
        let (text, _) = self.read_logical()?;
        if text.trim_start().starts_with('+') {
            self.write_all(bytes)?;
            return Ok(());
        }
        Err(imap_err(format!(
            "expected a command continuation, got {text}"
        )))
    }

    fn command(&mut self, label: &str, cmd: &str) -> Result<Reply, Error> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.write_all(format!("{tag} {cmd}\r\n").as_bytes())?;
        self.read_reply(&tag, label)
    }

    fn read_reply(&mut self, tag: &str, label: &str) -> Result<Reply, Error> {
        let mut untagged = Vec::new();
        let mut stored_bytes = 0usize;
        let mut overflow = false;
        loop {
            let (text, literals) = self.read_logical()?;
            let added = text.len() + literal_bytes(&literals);
            if stored_bytes.saturating_add(added) > command_byte_limit() {
                self.disconnect();
                return Err(imap_err(format!(
                    "{label} exceeded the {} byte reply limit",
                    command_byte_limit()
                )));
            }
            if text_starts_with_tag(&text, tag) {
                let rest = text[tag.len()..].trim_start();
                if overflow {
                    return Err(imap_err(format!(
                        "{label} exceeded {} untagged replies",
                        untagged_limit()
                    )));
                }
                let upper = rest.to_ascii_uppercase();
                if upper.starts_with("OK") {
                    return Ok(Reply {
                        untagged,
                        ok: rest.to_string(),
                    });
                }
                return Err(imap_err(format!("{label} failed: {rest}")));
            }
            self.note_untagged(&text);
            if overflow || untagged.len() >= untagged_limit() {
                overflow = true;
                continue;
            }
            stored_bytes += added;
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

    fn read_logical(&mut self) -> Result<(String, Vec<LiteralBytes>), Error> {
        self.read_logical_with_draft_budget(None)
    }

    fn read_logical_with_draft_budget(
        &mut self,
        draft_budget: Option<usize>,
    ) -> Result<(String, Vec<LiteralBytes>), Error> {
        let mut text = Vec::new();
        let mut literals = Vec::new();
        let mut retained = 0usize;
        loop {
            let line = self.read_until_crlf()?;
            match literal_marker(&line) {
                LiteralMarker::NotLiteral => {
                    if text.len().saturating_add(line.len()) > response_text_limit() {
                        self.disconnect();
                        return Err(self.response_cap_error());
                    }
                    text.extend_from_slice(&line);
                    break;
                }
                LiteralMarker::Unusable => {
                    self.disconnect();
                    return Err(imap_err(
                        "literal length is not a usable number; closing the connection",
                    ));
                }
                LiteralMarker::Length(len) => {
                    if draft_budget.is_some_and(|budget| {
                        retained.saturating_add(len) > budget || draft_size_exceeded(&line)
                    }) {
                        self.disconnect();
                        return Err(Error::DraftMessageLimit);
                    }
                    let too_big = len > literal_limit();
                    let over = text.len().saturating_add(line.len()) > response_text_limit()
                        || literals.len() >= response_literal_limit()
                        || (!too_big && retained.saturating_add(len) > response_limit());
                    if over {
                        // Stop reading. The socket is closed so the rest of this
                        // response cannot be parsed as later IMAP lines.
                        self.disconnect();
                        return Err(self.response_cap_error());
                    }
                    text.extend_from_slice(&line);
                    if too_big {
                        self.discard_exact(len)?;
                        literals.push(LiteralBytes::Discarded);
                    } else {
                        let bytes = self.read_exact(len)?;
                        retained += bytes.len();
                        literals.push(LiteralBytes::Kept(bytes));
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&text).into_owned();
        Ok((text, literals))
    }

    fn disconnect(&mut self) {
        self.io.take();
    }

    fn response_cap_error(&self) -> Error {
        imap_err(format!(
            "response literals exceed the {} byte or {} literal limit",
            response_limit(),
            response_literal_limit()
        ))
    }

    fn read_until_crlf(&mut self) -> Result<Vec<u8>, Error> {
        let mut buf = self.take_buf();
        let limit = line_limit();
        loop {
            if let Some(pos) = buf.iter().position(|byte| *byte == b'\n') {
                if pos > limit {
                    return Err(self.close_on_line_limit(limit));
                }
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
            if buf.len() > limit {
                return Err(self.close_on_line_limit(limit));
            }
            if !self.fill(&mut buf)? {
                return Err(imap_err(format!("connection to {} closed", self.host)));
            }
        }
    }

    fn close_on_line_limit(&mut self, limit: usize) -> Error {
        // The remainder of the line cannot be read as a new response.
        self.disconnect();
        Error::ImapLineLimit {
            host: self.host.clone(),
            limit,
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

    /// Drop `len` octets already promised by a literal, in fixed chunks, without
    /// keeping them. The rest of the response stays readable.
    fn discard_exact(&mut self, mut remaining: usize) -> Result<(), Error> {
        let mut buf = self.take_buf();
        let from_buf = remaining.min(buf.len());
        buf.drain(..from_buf);
        remaining -= from_buf;
        self.restore_buf(buf);
        let mut chunk = [0u8; DISCARD_CHUNK];
        while remaining > 0 {
            let want = remaining.min(chunk.len());
            let read = self.read_some(&mut chunk[..want])?;
            if read == 0 {
                return Err(imap_err(format!(
                    "connection to {} closed while discarding a literal",
                    self.host
                )));
            }
            remaining -= read;
        }
        Ok(())
    }

    fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let io = self
            .io
            .as_mut()
            .ok_or_else(|| imap_err("connection closed"))?;
        loop {
            match io.read(buf) {
                Ok(count) => return Ok(count),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    return Err(imap_err(format!("read from {} failed: {err}", self.host)));
                }
            }
        }
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

enum LiteralBytes {
    Kept(Vec<u8>),
    Discarded,
}

/// The UIDs in a `UID SEARCH` reply's `* SEARCH` lines, ascending.
fn search_uids(lines: &[Logical], limit: usize) -> Option<Vec<u32>> {
    let mut result = None;
    for line in lines {
        let mut words = line.text.split_whitespace();
        if words.next() != Some("*")
            || !words
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("SEARCH"))
        {
            continue;
        }
        let uids = result.get_or_insert_with(Vec::new);
        for word in words {
            match word.parse() {
                Ok(uid) if uids.len() < limit => uids.push(uid),
                Ok(_) => return None,
                // A modifier such as `(MODSEQ n)`, not a UID.
                Err(_) if word.starts_with('(') => break,
                Err(_) => return None,
            }
        }
    }
    result.map(|mut uids| {
        uids.sort_unstable();
        uids
    })
}

struct Logical {
    text: String,
    literals: Vec<LiteralBytes>,
}

struct Reply {
    untagged: Vec<Logical>,
    ok: String,
}

/// Largest RFC 822 payload this client will store. A bigger literal is discarded
/// and that message is skipped; the rest of the response is still read.
const MAX_LITERAL: usize = crate::MAX_MESSAGE_BYTES;

fn draft_size_exceeded(line: &[u8]) -> bool {
    String::from_utf8_lossy(line)
        .to_ascii_uppercase()
        .split_once(" RFC822.SIZE ")
        .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
        .is_some_and(|number| {
            !number.is_empty()
                && number
                    .parse::<usize>()
                    .map_or(true, |size| size > crate::MAX_MESSAGE_BYTES)
        })
}
const DISCARD_CHUNK: usize = 64 * 1024;
/// Protocol text before a newline, not including a `{n}` literal that follows.
const MAX_LINE: usize = 1024 * 1024;
/// Untagged replies stored for one tagged command. Stops a server that trickles
/// `* OK` lines and resets the read timeout on every chunk.
const MAX_UNTAGGED: usize = 1_000_000;
/// Bytes of one tagged command's replies that are kept. LIST, UID discovery,
/// and the flags pass have no message body, but they can still stream forever.
const MAX_COMMAND_BYTES: usize = 8 * 1024 * 1024;

/// Bytes of literals kept from one protocol response.
const MAX_RESPONSE: usize = MAX_LITERAL;
/// Header text kept while assembling one response. The bodies live in literals.
const MAX_RESPONSE_TEXT: usize = 1024 * 1024;
/// How many literals one response may contain. `{0}` adds no bytes, so a count
/// cap is what stops that chain.
const MAX_RESPONSE_LITERALS: usize = 32;

fn response_limit() -> usize {
    #[cfg(test)]
    {
        TEST_RESPONSE_LIMIT.with(|limit| limit.get())
    }
    #[cfg(not(test))]
    {
        MAX_RESPONSE
    }
}

fn response_text_limit() -> usize {
    MAX_RESPONSE_TEXT
}

fn response_literal_limit() -> usize {
    MAX_RESPONSE_LITERALS
}

fn command_byte_limit() -> usize {
    #[cfg(test)]
    {
        TEST_COMMAND_BYTES.with(|limit| limit.get())
    }
    #[cfg(not(test))]
    {
        MAX_COMMAND_BYTES
    }
}

fn literal_limit() -> usize {
    #[cfg(test)]
    {
        TEST_LITERAL_LIMIT.with(|limit| limit.get())
    }
    #[cfg(not(test))]
    {
        MAX_LITERAL
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LITERAL_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_LITERAL) };
    static TEST_RESPONSE_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_RESPONSE) };
    static TEST_COMMAND_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_COMMAND_BYTES) };
}

#[cfg(test)]
pub(super) struct LiteralLimitGuard;

#[cfg(test)]
pub(super) fn limit_literals_to(limit: usize) -> LiteralLimitGuard {
    TEST_LITERAL_LIMIT.with(|cell| cell.set(limit));
    LiteralLimitGuard
}

#[cfg(test)]
impl Drop for LiteralLimitGuard {
    fn drop(&mut self) {
        TEST_LITERAL_LIMIT.with(|cell| cell.set(MAX_LITERAL));
    }
}

#[cfg(test)]
struct ResponseLimitGuard;

#[cfg(test)]
fn limit_response_bytes_to(limit: usize) -> ResponseLimitGuard {
    TEST_RESPONSE_LIMIT.with(|cell| cell.set(limit));
    ResponseLimitGuard
}

#[cfg(test)]
impl Drop for ResponseLimitGuard {
    fn drop(&mut self) {
        TEST_RESPONSE_LIMIT.with(|cell| cell.set(MAX_RESPONSE));
    }
}

#[cfg(test)]
struct CommandByteGuard;

#[cfg(test)]
fn limit_command_bytes_to(limit: usize) -> CommandByteGuard {
    TEST_COMMAND_BYTES.with(|cell| cell.set(limit));
    CommandByteGuard
}

#[cfg(test)]
impl Drop for CommandByteGuard {
    fn drop(&mut self) {
        TEST_COMMAND_BYTES.with(|cell| cell.set(MAX_COMMAND_BYTES));
    }
}

fn line_limit() -> usize {
    #[cfg(test)]
    {
        TEST_LINE_LIMIT.with(|limit| limit.get())
    }
    #[cfg(not(test))]
    {
        MAX_LINE
    }
}

fn untagged_limit() -> usize {
    #[cfg(test)]
    {
        TEST_UNTAGGED_LIMIT.with(|limit| limit.get())
    }
    #[cfg(not(test))]
    {
        MAX_UNTAGGED
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LINE_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_LINE) };
    static TEST_UNTAGGED_LIMIT: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_UNTAGGED) };
}

#[cfg(test)]
struct LineLimitGuard;

#[cfg(test)]
fn limit_lines_to(limit: usize) -> LineLimitGuard {
    TEST_LINE_LIMIT.with(|cell| cell.set(limit));
    LineLimitGuard
}

#[cfg(test)]
impl Drop for LineLimitGuard {
    fn drop(&mut self) {
        TEST_LINE_LIMIT.with(|cell| cell.set(MAX_LINE));
    }
}

#[cfg(test)]
pub(super) struct UntaggedLimitGuard;

#[cfg(test)]
pub(super) fn limit_untagged_to(limit: usize) -> UntaggedLimitGuard {
    TEST_UNTAGGED_LIMIT.with(|cell| cell.set(limit));
    UntaggedLimitGuard
}

#[cfg(test)]
impl Drop for UntaggedLimitGuard {
    fn drop(&mut self) {
        TEST_UNTAGGED_LIMIT.with(|cell| cell.set(MAX_UNTAGGED));
    }
}

struct ParsedList {
    raw: String,
    decoded: String,
    noselect: bool,
    delimiter: Option<char>,
    attributes: Vec<String>,
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
    pending: Vec<u8>,
    config: &ImapConfig,
) -> Result<StreamOwned<ClientConnection, Prefixed<TcpStream>>, Error> {
    let tls = tls_config(config).map_err(|err| Error::Tls(err.to_string()))?;
    let name = ServerName::try_from(config.host.as_str())
        .map_err(|_| {
            Error::Tls(format!(
                "host {} is not a valid TLS server name",
                config.host
            ))
        })?
        .to_owned();
    let mut conn = ClientConnection::new(tls, name)
        .map_err(|err| Error::Tls(format!("TLS handshake setup failed: {err}")))?;
    // Bytes read with the tagged STARTTLS OK, including a ServerHello that
    // arrived in the same read, must be seen by rustls.
    let mut io = Prefixed {
        prefix: pending,
        pos: 0,
        inner: tcp,
    };
    conn.complete_io(&mut io)
        .map_err(|err| Error::Tls(format!("TLS handshake with {} failed: {err}", config.host)))?;
    if conn.is_handshaking() {
        return Err(Error::Tls(format!(
            "TLS handshake with {} did not complete",
            config.host
        )));
    }
    Ok(StreamOwned::new(conn, io))
}

/// Reads `prefix` before the underlying socket, then writes straight through.
struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S: Read> Read for Prefixed<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.prefix.len() {
            let count = (self.prefix.len() - self.pos).min(buf.len());
            buf[..count].copy_from_slice(&self.prefix[self.pos..self.pos + count]);
            self.pos += count;
            if self.pos == self.prefix.len() {
                self.prefix.clear();
                self.pos = 0;
            }
            return Ok(count);
        }
        self.inner.read(buf)
    }
}

impl<S: Write> Write for Prefixed<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn ring_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Cleartext TCP wrapped for SMTP STARTTLS. The caller has already read the
/// SMTP `220` that answered STARTTLS, so no prefix bytes are pending.
pub(crate) struct TlsStream {
    inner: StreamOwned<ClientConnection, Prefixed<TcpStream>>,
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn wrap_tcp_as_tls(
    tcp: TcpStream,
    host: &str,
    accept_invalid_certs: bool,
) -> Result<TlsStream, Error> {
    if accept_invalid_certs && !is_loopback_host(host) {
        return Err(imap_err(
            "accept_invalid_certs is only allowed for a loopback host",
        ));
    }
    let config = ImapConfig {
        host: host.to_string(),
        port: 0,
        username: String::new(),
        password: String::new(),
        security: ImapSecurity::Tls,
        accept_invalid_certs,
        folder_overrides: Vec::new(),
    };
    Ok(TlsStream {
        inner: wrap_tls(tcp, Vec::new(), &config)?,
    })
}

fn tls_config(config: &ImapConfig) -> Result<Arc<ClientConfig>, Error> {
    let provider = ring_provider();
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|err| imap_err(format!("tls config failed: {err}")))?;
    if config.accept_invalid_certs {
        let verifier = Arc::new(AcceptInvalidCerts { provider });
        return Ok(Arc::new(
            builder
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
        builder.with_root_certificates(roots).with_no_client_auth(),
    ))
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

pub(super) fn quote_imap(value: &str) -> Result<String, String> {
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

enum LiteralMarker {
    NotLiteral,
    Length(usize),
    /// `{...}` is a literal marker, but the length does not fit in `usize`.
    Unusable,
}

fn literal_bytes(literals: &[LiteralBytes]) -> usize {
    literals
        .iter()
        .map(|literal| match literal {
            LiteralBytes::Kept(bytes) => bytes.len(),
            LiteralBytes::Discarded => 0,
        })
        .sum()
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    // Only a literal address: a name such as `localhost` resolves at connect
    // time and may reach a machine that must not get an unchecked certificate.
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|addr| addr.is_loopback())
}

fn literal_marker(line: &[u8]) -> LiteralMarker {
    if !line.ends_with(b"}") {
        return LiteralMarker::NotLiteral;
    }
    let Some(start) = line.iter().rposition(|byte| *byte == b'{') else {
        return LiteralMarker::NotLiteral;
    };
    let mut number = &line[start + 1..line.len() - 1];
    if let Some(stripped) = number.strip_suffix(b"+") {
        number = stripped;
    }
    if number.is_empty() || !number.iter().all(u8::is_ascii_digit) {
        return LiteralMarker::NotLiteral;
    }
    match std::str::from_utf8(number)
        .ok()
        .and_then(|text| text.parse().ok())
    {
        Some(len) => LiteralMarker::Length(len),
        None => LiteralMarker::Unusable,
    }
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

/// Capabilities from a tagged OK that carries `[CAPABILITY ...]`.
fn capability_from_ok(ok: &str) -> Option<Vec<String>> {
    let upper = ok.to_ascii_uppercase();
    let marker = "[CAPABILITY ";
    let start = upper.find(marker)?;
    let rest = &ok[start + marker.len()..];
    let end = rest.find(']')?;
    let caps: Vec<String> = rest[..end].split_whitespace().map(str::to_string).collect();
    if caps.is_empty() { None } else { Some(caps) }
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

fn parse_list(text: &str, literals: &[LiteralBytes]) -> Option<ParsedList> {
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
    let delimiter = scan.astring()?;
    let delimiter = if delimiter.eq_ignore_ascii_case("NIL") {
        None
    } else {
        delimiter.chars().next()
    };
    let raw = scan.astring()?;
    let noselect = flags.iter().any(|flag| {
        flag.eq_ignore_ascii_case("\\Noselect") || flag.eq_ignore_ascii_case("\\NonExistent")
    });
    let decoded = decode_modified_utf7(&raw);
    Some(ParsedList {
        raw,
        decoded,
        noselect,
        delimiter,
        attributes: flags,
    })
}

fn parse_fetch(text: &str, literals: Vec<LiteralBytes>) -> Option<Fetched> {
    parse_fetch_with_section(text, literals, None)
}

fn parse_fetch_with_section(
    text: &str,
    mut literals: Vec<LiteralBytes>,
    section: Option<&str>,
) -> Option<Fetched> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('*') {
        return None;
    }
    let upper = text.to_ascii_uppercase();
    if !upper.contains(" FETCH ") && !upper.contains(" FETCH(") {
        return None;
    }
    let uid = number_after(&upper, "UID ")?;
    let flags = fetch_flags(text, &upper);
    let (body, skipped, body_missing) = body_after_body_key(text, &mut literals, section);
    Some(Fetched {
        uid,
        flags_present: flags.is_some(),
        flags: flags.unwrap_or_default(),
        modseq: fetch_modseq(&upper),
        body,
        skipped,
        body_missing,
        size: number_after(&upper, "RFC822.SIZE "),
        internal_date: fetch_internal_date(text, &upper),
    })
}

/// A `UID FLAGS` reply. Any literal the server sent along is dropped unread.
fn parse_flags(text: &str) -> Option<Fetched> {
    if !text.trim_start().starts_with('*') {
        return None;
    }
    let upper = text.to_ascii_uppercase();
    if !upper.contains(" FETCH ") && !upper.contains(" FETCH(") {
        return None;
    }
    Some(Fetched {
        uid: number_after(&upper, "UID ")?,
        flags: fetch_flags(text, &upper)?,
        flags_present: true,
        modseq: fetch_modseq(&upper),
        body: Vec::new(),
        skipped: false,
        body_missing: false,
        size: None,
        internal_date: None,
    })
}

fn fetch_modseq(upper: &str) -> Option<u64> {
    fetch_list(upper, upper, "MODSEQ")?.parse().ok()
}

fn fetch_flags(text: &str, upper: &str) -> Option<Vec<String>> {
    Some(
        fetch_list(text, upper, "FLAGS")?
            .split_whitespace()
            .map(str::to_string)
            .collect(),
    )
}

fn fetch_list<'a>(text: &'a str, upper: &str, key: &str) -> Option<&'a str> {
    let list = fetch_item(text, upper, key, '(')?;
    Some(list.split_once(')')?.0)
}

fn fetch_internal_date(text: &str, upper: &str) -> Option<i64> {
    let quoted = fetch_item(text, upper, "INTERNALDATE", '"')?;
    let value = quoted.split_once('"')?.0.trim_start();
    let (day, rest) = value.split_once('-')?;
    let (month, rest) = rest.split_once('-')?;
    let date = mail_parser::DateTime::parse_rfc822(&format!("{day} {month} {rest}"))?;
    super::rfc822::valid_date(&date)
}

fn fetch_number<T: std::str::FromStr>(text: &str, upper: &str, key: &str) -> Option<T> {
    let value = fetch_value(text, upper, key)?;
    value.split([' ', ')']).next()?.parse().ok()
}

fn literals_before(text: &str) -> usize {
    let mut quoted = false;
    let mut count = 0;
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' if quoted => {
                bytes.next();
            }
            b'"' => quoted = !quoted,
            b'{' if !quoted => count += 1,
            _ => {}
        }
    }
    count
}

fn fetch_item<'a>(text: &'a str, upper: &str, key: &str, delimiter: char) -> Option<&'a str> {
    fetch_value(text, upper, key)?.strip_prefix(delimiter)
}

fn fetch_value<'a>(text: &'a str, upper: &str, key: &str) -> Option<&'a str> {
    // A quoted body or a nested list can contain the key too. Literal bytes
    // are already out of band.
    let start = upper.find("FETCH")? + "FETCH".len();
    let items = &upper[start..];
    if !items.contains(key) {
        return None;
    }
    let mut depth = 0usize;
    let mut quoted = false;
    let mut bytes = items.bytes().enumerate();
    while let Some((index, byte)) = bytes.next() {
        match byte {
            b'\\' if quoted => {
                bytes.next();
            }
            b'"' => quoted = !quoted,
            _ if quoted => {}
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            first
                if depth == 1
                    && first == key.as_bytes()[0]
                    && matches!(items.as_bytes()[index - 1], b'(' | b' ')
                    && items[index..].starts_with(key)
                    && matches!(
                        items.as_bytes().get(index + key.len()),
                        Some(b' ' | b'(' | b'"')
                    ) =>
            {
                return Some(text[start + index + key.len()..].trim_start());
            }
            _ => {}
        }
    }
    None
}

/// The `<origin>` of a partial `BODY[<section>]<origin>` in a FETCH reply.
fn partial_origin(text: &str) -> Option<usize> {
    let upper = text.to_ascii_uppercase();
    let start = upper.find("BODY[")?;
    let close = start + upper[start..].find(']')?;
    let tail = text[close + 1..].strip_prefix('<')?;
    let (digits, _) = tail.split_once('>')?;
    if digits.is_empty() || digits.len() > 12 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The nstring after `BODY` / `BODY[...]`, not whichever literal arrived first.
fn body_after_body_key(
    text: &str,
    literals: &mut [LiteralBytes],
    section: Option<&str>,
) -> (Vec<u8>, bool, bool) {
    let upper = text.to_ascii_uppercase();
    let bytes = text.as_bytes();
    let mut search = 0;
    while let Some(rel) = upper[search..].find("BODY") {
        let index = search + rel;
        let after = index + 4;
        let boundary = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
        let next = bytes.get(after).copied();
        if !boundary || next == Some(b'S') || next == Some(b'.') {
            search = after;
            continue;
        }
        let mut cursor = after;
        if bytes.get(cursor) == Some(&b'[') {
            let Some(end) = text[cursor..].find(']').map(|end| cursor + end) else {
                return (Vec::new(), false, true);
            };
            if section.is_some_and(|section| upper[cursor + 1..end] != *section) {
                search = end + 1;
                continue;
            }
            cursor = end + 1;
            // A partial fetch names its origin octet: `BODY[2]<0>`.
            if bytes.get(cursor) == Some(&b'<') {
                if section.is_some() {
                    search = cursor;
                    continue;
                }
                let Some(end) = text[cursor..].find('>') else {
                    return (Vec::new(), false, true);
                };
                cursor += end + 1;
            }
        } else if section.is_some() {
            search = after;
            continue;
        }
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if text
            .get(cursor..cursor + 3)
            .is_some_and(|word| word.eq_ignore_ascii_case("NIL"))
        {
            let boundary = bytes
                .get(cursor + 3)
                .is_none_or(|byte| !byte.is_ascii_alphanumeric());
            if boundary {
                return (Vec::new(), false, false);
            }
        }
        if bytes.get(cursor) == Some(&b'"') {
            return match imap_quoted(&text[cursor..]) {
                Some(body) => (body, false, false),
                None => (Vec::new(), false, true),
            };
        }
        if bytes.get(cursor) == Some(&b'{') {
            let slot = literal_slot_index(text, cursor);
            return match literals.get_mut(slot) {
                Some(literal) => match std::mem::replace(literal, LiteralBytes::Discarded) {
                    LiteralBytes::Kept(body) => (body, false, false),
                    LiteralBytes::Discarded => (Vec::new(), true, false),
                },
                None => (Vec::new(), false, true),
            };
        }
        return (Vec::new(), false, true);
    }
    (Vec::new(), false, true)
}

fn imap_quoted(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                index += 1;
                out.push(*bytes.get(index)?);
            }
            b'"' => return Some(out),
            byte => out.push(byte),
        }
        index += 1;
    }
    None
}

fn literal_slot_index(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut count = 0;
    while index < at && index < bytes.len() {
        if bytes[index] == b'"' {
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\\' {
                    index += 2;
                    continue;
                }
                if bytes[index] == b'"' {
                    index += 1;
                    break;
                }
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'{'
            && let Some(token) = literal_token_len(&text[index..])
        {
            count += 1;
            index += token;
            continue;
        }
        index += 1;
    }
    count
}

fn literal_token_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let end = bytes.iter().position(|byte| *byte == b'}')?;
    let mut inner = &bytes[1..end];
    if inner.last() == Some(&b'+') {
        inner = &inner[..inner.len() - 1];
    }
    if inner.is_empty() || !inner.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(end + 1)
}

fn uid_in_fetch_text(text: &str) -> Option<u32> {
    let upper = text.to_ascii_uppercase();
    if !upper.contains("FETCH") {
        return None;
    }
    number_after(&upper, "UID ")
}

fn number_after(upper: &str, key: &str) -> Option<u32> {
    let index = upper.find(key)?;
    let after = upper[index + key.len()..].trim_start();
    let end = after
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(after.len());
    after[..end].parse().ok()
}

fn strip_prefix_ignore_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

struct Scan<'a> {
    text: &'a str,
    index: usize,
    literals: &'a [LiteralBytes],
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
            let bytes = match self.literals.get(self.literal)? {
                LiteralBytes::Kept(bytes) => bytes,
                LiteralBytes::Discarded => return None,
            };
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

fn is_bye(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line).to_ascii_uppercase();
    let mut words = text.split_whitespace();
    words.next() == Some("*") && words.next() == Some("BYE")
}

/// An untagged IDLE line that means the mailbox changed.
fn idle_news(line: &[u8]) -> bool {
    let text = String::from_utf8_lossy(line).to_ascii_uppercase();
    let mut words = text.split_whitespace();
    if words.next() != Some("*") {
        return false;
    }
    let Some(number) = words.next() else {
        return false;
    };
    // With QRESYNC enabled, an expunge arrives as `* VANISHED uids`.
    if number == "VANISHED" {
        return true;
    }
    number.chars().all(|ch| ch.is_ascii_digit())
        && matches!(
            words.next(),
            Some("EXISTS" | "EXPUNGE" | "RECENT" | "FETCH")
        )
}

/// `* ENABLED ...` that names `capability`.
fn enabled_lists(text: &str, capability: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    let mut words = upper.split_whitespace();
    words.next() == Some("*")
        && words.next() == Some("ENABLED")
        && words.any(|word| word.eq_ignore_ascii_case(capability))
}

/// An IMAP sequence set of UIDs (`1:3,7,9`) as inclusive ranges.
///
/// `None` for a set with `*` (RFC 7162 allows it; it means the highest UID
/// in the mailbox, which may be a message that is still there) or one that
/// does not parse. Dropping copies from such a set could drop live mail.
fn parse_uid_set(set: &str) -> Option<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    let token = set.split_whitespace().next().unwrap_or("");
    for part in token.split(',') {
        let number = |raw: &str| -> Option<u32> { raw.parse().ok().filter(|uid| *uid > 0) };
        let (low, high) = match part.split_once(':') {
            Some((low, high)) => (number(low)?, number(high)?),
            None => {
                let uid = number(part)?;
                (uid, uid)
            }
        };
        out.push((low.min(high), low.max(high)));
    }
    Some(out)
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
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};

    use super::{connect_addrs, parse_uid_set};

    #[test]
    fn oversized_literal_is_discarded_and_the_response_continues() {
        use std::io::{Read, Write};
        use std::thread;

        use crate::imap::{ImapConfig, ImapSecurity};

        let _limit = super::limit_literals_to(8);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"* OK hi\r\n").unwrap();
            let line = read_imap_command(&mut sock);
            let tag = line.split_whitespace().next().unwrap_or("a1");
            sock.write_all(b"* CAPABILITY {24}\r\n").unwrap();
            sock.write_all(b"xxxxxxxxxxxxxxxxxxxxxxxx\r\n").unwrap();
            sock.write_all(format!("{tag} OK [CAPABILITY IMAP4rev1] LOGIN\r\n").as_bytes())
                .unwrap();
            let _ = sock.read(&mut [0u8; 64]);
        });
        super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        })
        .expect("an oversized literal is discarded so the rest of the response can finish");
    }

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

    #[test]
    fn response_line_longer_than_the_cap_is_rejected() {
        use std::io::Write;
        use std::thread;

        use crate::imap::{ImapConfig, ImapSecurity};

        let _limit = super::limit_lines_to(32);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut greeting = b"* OK ".to_vec();
            greeting.extend(std::iter::repeat_n(b'A', 200));
            greeting.extend(b"\r\n");
            sock.write_all(&greeting).unwrap();
        });
        let opened = super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        });
        let Err(err) = opened else {
            panic!("an unbounded greeting was accepted");
        };
        let text = err.to_string();
        assert!(text.contains("limit"), "{text}");
        assert!(text.contains("32"), "{text}");
    }

    #[test]
    fn multibyte_list_line_is_skipped() {
        use std::thread;

        use crate::imap::{ImapConfig, ImapSecurity};

        assert!(super::parse_list("* €é", &[]).is_none());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"* OK hi\r\n").unwrap();
            loop {
                let line = read_imap_command(&mut sock);
                if line.is_empty() {
                    break;
                }
                let tag = line.split_whitespace().next().unwrap_or("a1");
                let upper = line.to_ascii_uppercase();
                let response = if upper.contains("LOGIN") {
                    format!("{tag} OK [CAPABILITY IMAP4rev1] LOGIN\r\n")
                } else if upper.contains("LIST") {
                    format!("* €é\r\n* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n{tag} OK LIST\r\n")
                } else {
                    format!("{tag} OK\r\n")
                };
                if sock.write_all(response.as_bytes()).is_err() {
                    break;
                }
            }
        });
        let mut session = super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        })
        .expect("login");
        let listed = session.list_selectable().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].decoded, "INBOX");
    }

    #[test]
    fn quoted_nil_and_later_literal_bodies_are_distinct() {
        let quoted = super::parse_fetch(
            r#"* 1 FETCH (UID 3 FLAGS (\Seen) BODY[] "hello")"#,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(quoted.body, b"hello");
        assert!(!quoted.body_missing);
        assert!(!quoted.skipped);

        let nil =
            super::parse_fetch("* 1 FETCH (UID 3 FLAGS (\\Seen) BODY[] NIL)", Vec::new()).unwrap();
        assert!(nil.body.is_empty());
        assert!(!nil.body_missing);

        let flagged = super::parse_fetch(
            r#"* 1 FETCH (UID 7 FLAGS {5} BODY[] "hello")"#,
            vec![super::LiteralBytes::Kept(br"\Seen".to_vec())],
        )
        .unwrap();
        assert_eq!(flagged.body, b"hello");
        assert!(!flagged.body_missing);

        let literal = super::parse_fetch(
            "* 1 FETCH (UID 1 FLAGS (\\Seen) BODY[] {5})",
            vec![super::LiteralBytes::Kept(b"hello".to_vec())],
        )
        .unwrap();
        assert_eq!(literal.body, b"hello");

        let missing = super::parse_fetch("* 1 FETCH (UID 4 FLAGS (\\Seen))", Vec::new()).unwrap();
        assert!(missing.body_missing);
        assert!(missing.body.is_empty());

        let multibyte = super::parse_fetch("* 1 FETCH (UID 5 FLAGS () BODY[] éé)", Vec::new())
            .expect("a multibyte tail is not NIL");
        assert!(multibyte.body_missing);

        let quoted_without_flags = super::parse_fetch(
            "* 1 FETCH (UID 5 BODY[] \"text with FLAGS (false-keyword)\")",
            Vec::new(),
        )
        .unwrap();
        assert!(!quoted_without_flags.flags_present);
        assert!(multibyte.flags_present);
    }

    #[test]
    fn a_reply_to_fetch_reads_the_requested_section_and_top_level_keyword_flags() {
        use super::{LiteralBytes, REPLY_TO_SECTION, parse_fetch_with_section};

        let header = b"Reply-To: list@lists.test\r\n\r\n";
        let response = format!(
            r"* 1 FETCH (UID 7 FLAGS (\Seen project) BODY[] {{5}} BODY[{REPLY_TO_SECTION}] {{{}}})",
            header.len(),
        );
        let item = parse_fetch_with_section(
            &response,
            vec![
                LiteralBytes::Kept(b"other".to_vec()),
                LiteralBytes::Kept(header.to_vec()),
            ],
            Some(REPLY_TO_SECTION),
        )
        .unwrap();
        assert!(item.flags_present);
        assert_eq!(item.flags, [r"\Seen", "project"]);
        assert_eq!(item.body, header);

        let item = parse_fetch_with_section(
            r#"* 1 FETCH (UID 7 BODY[] "text with FLAGS (false-keyword)")"#,
            Vec::new(),
            Some(REPLY_TO_SECTION),
        )
        .unwrap();
        assert!(!item.flags_present);
        assert!(item.body_missing);
    }

    #[test]
    fn internaldate_is_a_top_level_fetch_item_with_a_numeric_zone() {
        let parse = |text| super::parse_fetch(text, Vec::new()).unwrap().internal_date;
        assert_eq!(
            parse(r#"* 1 FETCH (UID 7 INTERNALDATE " 9-Feb-2024 17:00:00 -0500")"#),
            Some(1_707_516_000)
        );
        assert_eq!(
            parse(
                r#"* 1 FETCH (UID 7 BODY[] "text with INTERNALDATE \"09-Feb-2024 22:00:00 +0000\"")"#
            ),
            None
        );
        assert_eq!(
            parse(r#"* 1 FETCH (UID 7 INTERNALDATE "31-Feb-2024 17:00:00 -0500")"#),
            None
        );
    }

    #[test]
    fn command_reply_byte_cap_closes_the_socket() {
        use std::io::{Read, Write};
        use std::thread;
        use std::time::{Duration, Instant};

        use crate::imap::{ImapConfig, ImapSecurity};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"* OK hi\r\n").unwrap();
            let line = read_imap_command(&mut sock);
            let tag = line.split_whitespace().next().unwrap_or("a1");
            sock.write_all(format!("{tag} OK [CAPABILITY IMAP4rev1] LOGIN\r\n").as_bytes())
                .unwrap();
            let line = read_imap_command(&mut sock);
            let tag = line.split_whitespace().next().unwrap_or("a2");
            sock.write_all(b"* LIST (\\HasNoChildren) \"/\" \"this-mailbox-name-is-long\"\r\n")
                .unwrap();
            let _ = sock.flush();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let _ = sock.read(&mut [0u8; 1]);
            let _ = sock.write_all(format!("{tag} OK LIST\r\n").as_bytes());
        });
        let mut session = super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        })
        .expect("login");
        let _limit = super::limit_command_bytes_to(24);
        let started = Instant::now();
        let listed = session.list_selectable();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the command cap must close the socket instead of buffering the rest"
        );
        let Err(err) = listed else {
            panic!("an unbounded LIST reply was stored");
        };
        let text = err.to_string();
        assert!(text.contains("byte reply limit"), "{text}");
        assert!(text.contains("24"), "{text}");
    }

    #[test]
    fn invalid_certs_are_rejected_off_loopback() {
        use crate::imap::{ImapConfig, ImapSecurity};

        let opened = super::Session::open(&ImapConfig {
            host: "imap.example.com".into(),
            port: 993,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Tls,
            accept_invalid_certs: true,
            folder_overrides: Vec::new(),
        });
        let Err(err) = opened else {
            panic!("invalid certificates were accepted for a public host");
        };
        let text = err.to_string();
        assert!(text.contains("loopback"), "{text}");
    }

    #[test]
    fn unusable_literal_length_stops_the_session() {
        use std::io::Write;
        use std::thread;

        use crate::imap::{ImapConfig, ImapSecurity};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"* OK {999999999999999999999}\r\n").unwrap();
            sock.write_all(b"not-a-line\r\n").unwrap();
        });
        let opened = super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        });
        let Err(err) = opened else {
            panic!("an unusable literal length was treated as protocol text");
        };
        let text = err.to_string();
        assert!(text.contains("literal length"), "{text}");
    }

    #[test]
    fn chained_literals_cannot_grow_without_a_total_cap() {
        use std::io::Write;
        use std::thread;

        use crate::imap::{ImapConfig, ImapSecurity};

        let _limit = super::limit_response_bytes_to(10);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"* OK {8}\r\n").unwrap();
            sock.write_all(b"IMAP4rev1").unwrap();
            sock.write_all(b" {8}\r\n").unwrap();
            sock.write_all(b"IMAP4rev1\r\n").unwrap();
        });
        let opened = super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        });
        let Err(err) = opened else {
            panic!("chained literals were retained without a total cap");
        };
        let text = err.to_string();
        assert!(text.contains("response literals"), "{text}");
        assert!(text.contains("10"), "{text}");
    }

    #[test]
    fn zero_length_literal_chain_is_rejected() {
        use std::io::Write;
        use std::thread;

        use crate::imap::{ImapConfig, ImapSecurity};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"* OK {0}\r\n").unwrap();
            for _ in 0..40 {
                sock.write_all(b" {0}\r\n").unwrap();
            }
            sock.write_all(b"\r\n").unwrap();
        });
        let opened = super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        });
        let Err(err) = opened else {
            panic!("a chain of empty literals was accepted");
        };
        let text = err.to_string();
        assert!(text.contains("literal limit"), "{text}");
    }

    #[test]
    fn response_cap_closes_without_waiting_for_the_rest() {
        use std::io::{Read, Write};
        use std::thread;
        use std::time::{Duration, Instant};

        use crate::imap::{ImapConfig, ImapSecurity};

        let _limit = super::limit_response_bytes_to(10);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"* OK {8}\r\nIMAP4rev1 {8}\r\n").unwrap();
            let _ = sock.flush();
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let _ = sock.read(&mut [0u8; 1]);
        });
        let started = Instant::now();
        let opened = super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        });
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the cap must close the socket instead of waiting for the rest of the reply"
        );
        let Err(err) = opened else {
            panic!("the over-cap literal was kept");
        };
        let text = err.to_string();
        assert!(text.contains("response literals"), "{text}");
        assert!(text.contains("10"), "{text}");
    }

    #[test]
    fn too_many_untagged_replies_are_rejected() {
        use std::io::Write;
        use std::thread;

        use crate::imap::{ImapConfig, ImapSecurity};

        let _limit = super::limit_untagged_to(2);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"* OK hi\r\n").unwrap();
            let line = read_imap_command(&mut sock);
            let tag = line.split_whitespace().next().unwrap_or("a1");
            sock.write_all(b"* OK one\r\n* OK two\r\n* OK three\r\n")
                .unwrap();
            sock.write_all(format!("{tag} OK [CAPABILITY IMAP4rev1] LOGIN\r\n").as_bytes())
                .unwrap();
        });
        let opened = super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security: ImapSecurity::Plain,
            accept_invalid_certs: false,
            folder_overrides: Vec::new(),
        });
        let Err(err) = opened else {
            panic!("an untagged flood was accepted");
        };
        let text = err.to_string();
        assert!(text.contains("untagged"), "{text}");
        assert!(text.contains('2'), "{text}");
    }

    #[test]
    fn a_vanished_set_with_star_is_not_read_as_uids() {
        assert_eq!(
            parse_uid_set("1:3,7,9:8"),
            Some(vec![(1, 3), (7, 7), (8, 9)])
        );
        assert_eq!(parse_uid_set("2:*"), None);
        assert_eq!(parse_uid_set("4,*"), None);
        assert_eq!(parse_uid_set("4,x"), None);
        assert_eq!(parse_uid_set("0:3"), None);
    }

    #[test]
    fn starttls_then_login_succeeds() {
        login_over_tls(crate::imap::ImapSecurity::StartTls);
    }

    #[test]
    fn implicit_tls_then_login_succeeds() {
        login_over_tls(crate::imap::ImapSecurity::Tls);
    }

    fn login_over_tls(security: crate::imap::ImapSecurity) {
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use rustls::{ServerConfig, ServerConnection, StreamOwned};

        use crate::imap::ImapConfig;

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certs = vec![CertificateDer::from(certified.cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let saw_login = Arc::new(AtomicBool::new(false));
        let login_flag = Arc::clone(&saw_login);
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            if security == crate::imap::ImapSecurity::StartTls {
                sock.write_all(b"* OK ready\r\n").unwrap();
                let line = read_imap_command(&mut sock);
                let tag = line.split_whitespace().next().unwrap_or("a1");
                sock.write_all(format!("{tag} OK STARTTLS\r\n").as_bytes())
                    .unwrap();
            }
            let conn = ServerConnection::new(Arc::new(server_config)).unwrap();
            let mut tls = StreamOwned::new(conn, sock);
            if security == crate::imap::ImapSecurity::Tls {
                tls.write_all(b"* OK ready\r\n").unwrap();
            }
            loop {
                let line = read_imap_command(&mut tls);
                if line.is_empty() {
                    break;
                }
                let tag = line.split_whitespace().next().unwrap_or("*");
                let upper = line.to_ascii_uppercase();
                let response = if upper.contains("LOGIN") {
                    login_flag.store(true, Ordering::SeqCst);
                    format!("{tag} OK [CAPABILITY IMAP4rev1] LOGIN\r\n")
                } else if upper.contains("CAPABILITY") {
                    format!("* CAPABILITY IMAP4rev1\r\n{tag} OK\r\n")
                } else {
                    format!("{tag} OK\r\n")
                };
                if tls.write_all(response.as_bytes()).is_err() {
                    break;
                }
            }
        });
        super::Session::open(&ImapConfig {
            host: "127.0.0.1".into(),
            port,
            username: "caleb".into(),
            password: "secret".into(),
            security,
            accept_invalid_certs: true,
            folder_overrides: Vec::new(),
        })
        .expect("TLS login");
        assert!(
            saw_login.load(Ordering::SeqCst),
            "LOGIN must be read on the TLS stream, which means the handshake finished"
        );
    }

    fn read_imap_command(stream: &mut (impl Read + std::io::Write)) -> String {
        let mut line = String::new();
        loop {
            let chunk = read_imap_line(stream);
            if chunk.is_empty() && line.is_empty() {
                return String::new();
            }
            line.push_str(chunk.trim_end_matches(['\r', '\n']));
            let Some(len) = command_literal_len(&line) else {
                return line;
            };
            if let Some(start) = line.rfind('{') {
                line.truncate(start);
            }
            stream.write_all(b"+ continue\r\n").unwrap();
            stream.flush().unwrap();
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).unwrap();
            line.push_str(&String::from_utf8_lossy(&buf));
        }
    }

    fn command_literal_len(line: &str) -> Option<usize> {
        if !line.ends_with('}') {
            return None;
        }
        let start = line.rfind('{')?;
        let inner = &line[start + 1..line.len() - 1];
        if inner.is_empty() || !inner.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        inner.parse().ok()
    }

    fn read_imap_line(stream: &mut impl Read) -> String {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if stream.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
        String::from_utf8_lossy(&line).into_owned()
    }
}
