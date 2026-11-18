//! In-process IMAP server for tests. Speaks the read-only subset the client uses.
//!
//! Tests change the mailboxes between syncs to stand in for the server: a
//! message dropped from `messages` was expunged, one pushed with a new UID
//! arrived. Give `uidnext` when the highest UID goes, as a real server keeps
//! UIDNEXT where it was.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum BodyForm {
    #[default]
    Literal,
    Quoted,
    Nil,
    Absent,
}

pub(super) struct FixtureMessage {
    pub uid: u32,
    pub flags: Vec<String>,
    pub modseq: u64,
    pub raw: Vec<u8>,
    pub body_form: BodyForm,
}

pub(super) struct FixtureMailbox {
    pub name: String,
    pub list_flags: String,
    pub uidvalidity: u32,
    /// When set, EXAMINE reports this UIDNEXT instead of one past the highest UID.
    pub uidnext: Option<u32>,
    /// Omit UIDNEXT even when `uidnext` is set. Forces the UID-search path.
    pub omit_uidnext: bool,
    /// LIST this mailbox, but answer EXAMINE with NO.
    pub reject_examine: bool,
    /// EXAMINE succeeds, but UID FETCH answers NO/BAD.
    pub reject_fetch: bool,
    pub messages: Vec<FixtureMessage>,
    /// Expunged UIDs and the modseq of each expunge, for QRESYNC `VANISHED`.
    pub vanished: Vec<(u32, u64)>,
}

pub(super) struct TestState {
    pub username: String,
    pub password: String,
    pub condstore: bool,
    pub close_after_starttls: bool,
    /// Drop the socket when a UID command arrives, with no tagged reply.
    pub close_during_uid_fetch: bool,
    /// Advertise UTF8=ACCEPT and allow ENABLE before authentication.
    pub utf8_accept: bool,
    /// Reject a non-ASCII password until UTF8=ACCEPT has been enabled.
    pub require_utf8_for_login: bool,
    /// Close the socket after this many BODY FETCH replies, with no tagged OK.
    pub close_after_body_fetches: Option<u32>,
    pub mailboxes: Vec<FixtureMailbox>,
    pub commands: Vec<String>,
    /// Advertise IDLE and answer it.
    pub idle: bool,
    /// While a client idles, send `* N EXISTS` once, then clear.
    pub idle_push: bool,
    /// Send `* N EXISTS` in the same write as, and before, `+ idling`.
    pub idle_news_first: bool,
    /// End IDLE at once with a tagged BAD after the continuation.
    pub idle_bad: bool,
    /// Advertise QRESYNC (RFC 7162) and answer `VANISHED` fetches.
    pub qresync: bool,
    /// Announce `* 1 EXPUNGE` before the next UID SEARCH reply, once.
    pub expunge_during_search: bool,
    /// Answer every `VANISHED` fetch with `* VANISHED (EARLIER) {set}`,
    /// verbatim, instead of the UIDs in `vanished`.
    pub vanished_reply: Option<String>,
    /// Advertise SPECIAL-USE (RFC 6154). A plain LIST then leaves the role
    /// attributes out, as a server may; `RETURN (SPECIAL-USE)` gets them.
    /// Without it, `list_flags` go out on every LIST as written.
    pub special_use: bool,
    /// Advertise SPECIAL-USE but answer `RETURN (SPECIAL-USE)` with BAD.
    pub reject_list_return: bool,
}

pub(super) struct TestServer {
    pub addr: std::net::SocketAddr,
    pub state: Arc<Mutex<TestState>>,
    /// Bytes the server has written to clients, greetings and replies alike.
    pub sent: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    pub(super) fn spawn(state: TestState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(state));
        let shutdown = Arc::new(AtomicBool::new(false));
        let sent = Arc::new(AtomicUsize::new(0));
        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_sent = Arc::clone(&sent);
        let thread = thread::spawn(move || {
            accept_loop(listener, thread_state, thread_shutdown, thread_sent)
        });
        Self {
            addr,
            state,
            sent,
            shutdown,
            thread: Some(thread),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    state: Arc<Mutex<TestState>>,
    shutdown: Arc<AtomicBool>,
    sent: Arc<AtomicUsize>,
) {
    listener.set_nonblocking(false).expect("blocking");
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                handle(stream, &state, &sent);
            }
            Err(_) => break,
        }
    }
}

/// The server's half of a connection, counting the bytes it sends.
struct Counted {
    stream: TcpStream,
    sent: Arc<AtomicUsize>,
}

impl Write for Counted {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.stream.write(buf)?;
        self.sent.fetch_add(written, Ordering::Relaxed);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

fn handle(stream: TcpStream, state: &Arc<Mutex<TestState>>, sent: &Arc<AtomicUsize>) {
    let mut write = Counted {
        stream: stream.try_clone().expect("clone"),
        sent: Arc::clone(sent),
    };
    let mut read = BufReader::new(stream);
    let _ = write.write_all(b"* OK fell-test ready\r\n");
    let _ = write.flush();
    let mut authed = false;
    let mut condstore_enabled = false;
    let mut utf8_enabled = false;
    let mut selected_condstore = false;
    let mut qresync_enabled = false;
    while let Some((line, literals)) = read_client_command(&mut read, &mut write) {
        if line.is_empty() {
            continue;
        }
        let Some((tag, verb, rest)) = split_command(&line) else {
            let _ = write.write_all(b"* BAD unparsed\r\n");
            continue;
        };
        {
            let mut guard = state.lock().expect("state");
            if verb.eq_ignore_ascii_case("LOGIN") {
                let kind = if literals.is_empty() {
                    "LOGIN"
                } else {
                    "LOGIN literal"
                };
                guard.commands.push(kind.into());
            } else {
                guard.commands.push(line.clone());
            }
        }
        let verb_upper = verb.to_ascii_uppercase();
        match verb_upper.as_str() {
            "CAPABILITY" => {
                let caps = capability_text(&state.lock().expect("state"));
                let _ = write.write_all(
                    format!("* CAPABILITY {caps}\r\n{tag} OK CAPABILITY\r\n").as_bytes(),
                );
            }
            "ENABLE" => {
                let upper = rest.to_ascii_uppercase();
                let wants_utf8 = upper.contains("UTF8=ACCEPT");
                let wants_qresync = upper.contains("QRESYNC");
                let wants_condstore = upper.contains("CONDSTORE");
                if wants_qresync {
                    let offered = state.lock().expect("state").qresync;
                    if authed && offered {
                        qresync_enabled = true;
                        condstore_enabled = true;
                        let _ = write.write_all(
                            format!("* ENABLED QRESYNC\r\n{tag} OK ENABLE\r\n").as_bytes(),
                        );
                    } else {
                        let _ = write.write_all(format!("{tag} OK ENABLE\r\n").as_bytes());
                    }
                    let _ = write.flush();
                    continue;
                }
                if !authed && !(wants_utf8 && state.lock().expect("state").utf8_accept) {
                    let _ = write.write_all(
                        format!("{tag} NO ENABLE is only valid in the authenticated state\r\n")
                            .as_bytes(),
                    );
                } else {
                    if wants_utf8 {
                        utf8_enabled = true;
                    }
                    if wants_condstore && authed {
                        condstore_enabled = true;
                    }
                    let enabled = if wants_utf8 && wants_condstore {
                        "UTF8=ACCEPT CONDSTORE"
                    } else if wants_utf8 {
                        "UTF8=ACCEPT"
                    } else {
                        "CONDSTORE"
                    };
                    let _ = write.write_all(
                        format!("* ENABLED {enabled}\r\n{tag} OK ENABLE\r\n").as_bytes(),
                    );
                }
            }
            "STARTTLS" => {
                let _ = write.write_all(format!("{tag} OK STARTTLS\r\n").as_bytes());
                let _ = write.flush();
                if state.lock().expect("state").close_after_starttls {
                    break;
                }
            }
            "LOGIN" => {
                let (user, pass) = if literals.len() >= 2 {
                    (
                        String::from_utf8_lossy(&literals[0]).into_owned(),
                        String::from_utf8_lossy(&literals[1]).into_owned(),
                    )
                } else {
                    login_args(&rest)
                };
                let guard = state.lock().expect("state");
                let non_ascii = pass.bytes().any(|byte| byte > 127);
                if guard.require_utf8_for_login && non_ascii && !utf8_enabled {
                    let _ = write.write_all(
                        format!("{tag} NO [CLIENTBUG] UTF8=ACCEPT required\r\n").as_bytes(),
                    );
                } else if user == guard.username && pass == guard.password {
                    authed = true;
                    let caps = capability_text(&guard);
                    let _ = write
                        .write_all(format!("{tag} OK [CAPABILITY {caps}] LOGIN\r\n").as_bytes());
                } else {
                    let _ = write.write_all(
                        format!("{tag} NO [AUTHENTICATIONFAILED] login failed\r\n").as_bytes(),
                    );
                }
            }
            "LIST" => {
                let guard = state.lock().expect("state");
                let asks_roles = rest.to_ascii_uppercase().contains("RETURN (SPECIAL-USE)");
                if asks_roles && (!guard.special_use || guard.reject_list_return) {
                    let _ =
                        write.write_all(format!("{tag} BAD unknown LIST option\r\n").as_bytes());
                    continue;
                }
                for mailbox in &guard.mailboxes {
                    let mut flags = mailbox.list_flags.clone();
                    if guard.special_use && !asks_roles {
                        flags = flags
                            .split_whitespace()
                            .filter(|flag| !is_special_use(flag))
                            .collect::<Vec<_>>()
                            .join(" ");
                    }
                    if flags.is_empty() {
                        flags = "\\HasNoChildren".to_string();
                    }
                    if mailbox_needs_literal(&mailbox.name) {
                        let _ = write.write_all(
                            format!("* LIST ({flags}) \"/\" {{{}}}\r\n", mailbox.name.len())
                                .as_bytes(),
                        );
                        let _ = write.write_all(mailbox.name.as_bytes());
                        let _ = write.write_all(b"\r\n");
                    } else {
                        let name = quote(&mailbox.name);
                        let _ = write
                            .write_all(format!("* LIST ({flags}) \"/\" {name}\r\n").as_bytes());
                    }
                }
                let _ = write.write_all(format!("{tag} OK LIST\r\n").as_bytes());
            }
            "EXAMINE" => {
                let name = mailbox_name(rest.trim());
                selected_condstore = rest.to_ascii_uppercase().contains("CONDSTORE");
                let guard = state.lock().expect("state");
                let Some(mailbox) = guard.mailboxes.iter().find(|item| item.name == name) else {
                    let _ = write.write_all(format!("{tag} NO no such mailbox\r\n").as_bytes());
                    continue;
                };
                if mailbox.reject_examine {
                    let _ = write.write_all(
                        format!("{tag} NO EXAMINE failed: permission denied\r\n").as_bytes(),
                    );
                    continue;
                }
                let exists = mailbox.messages.len();
                let highest = mailbox
                    .messages
                    .iter()
                    .map(|message| message.uid)
                    .max()
                    .unwrap_or(0);
                let next = mailbox.uidnext.unwrap_or(highest.saturating_add(1));
                let _ = write.write_all(format!("* {exists} EXISTS\r\n").as_bytes());
                let _ = write.write_all(
                    format!("* OK [UIDVALIDITY {}] UIDs valid\r\n", mailbox.uidvalidity).as_bytes(),
                );
                if !mailbox.omit_uidnext {
                    let _ = write.write_all(
                        format!("* OK [UIDNEXT {next}] Predicted next UID\r\n").as_bytes(),
                    );
                }
                if guard.condstore && (condstore_enabled || selected_condstore) {
                    let highest = mailbox
                        .messages
                        .iter()
                        .map(|message| message.modseq)
                        .chain(mailbox.vanished.iter().map(|(_, modseq)| *modseq))
                        .max()
                        .unwrap_or(1);
                    let _ = write.write_all(
                        format!("* OK [HIGHESTMODSEQ {highest}] highest\r\n").as_bytes(),
                    );
                }
                let _ = write
                    .write_all(format!("{tag} OK [READ-ONLY] EXAMINE completed\r\n").as_bytes());
            }
            "UID" if strip_prefix_ignore(rest.trim(), "SEARCH").is_some() => {
                let mut guard = state.lock().expect("state");
                let Some(name) = last_examined(&guard.commands) else {
                    let _ = write.write_all(format!("{tag} BAD no mailbox\r\n").as_bytes());
                    continue;
                };
                if std::mem::take(&mut guard.expunge_during_search) {
                    let _ = write.write_all(b"* 1 EXPUNGE\r\n");
                }
                let Some(mailbox) = guard.mailboxes.iter().find(|item| item.name == name) else {
                    let _ = write.write_all(format!("{tag} NO no mailbox\r\n").as_bytes());
                    continue;
                };
                let range = strip_prefix_ignore(rest.trim(), "SEARCH")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let count = mailbox.messages.len() as u32;
                let mut line = String::from("* SEARCH");
                for (index, message) in mailbox.messages.iter().enumerate() {
                    if uid_in_range(index as u32 + 1, &range, count) {
                        line.push_str(&format!(" {}", message.uid));
                    }
                }
                let _ = write.write_all(format!("{line}\r\n{tag} OK SEARCH\r\n").as_bytes());
            }
            "FETCH" => {
                let guard = state.lock().expect("state");
                let Some(name) = last_examined(&guard.commands) else {
                    let _ = write.write_all(format!("{tag} BAD no mailbox\r\n").as_bytes());
                    continue;
                };
                let Some(mailbox) = guard.mailboxes.iter().find(|item| item.name == name) else {
                    let _ = write.write_all(format!("{tag} NO no mailbox\r\n").as_bytes());
                    continue;
                };
                if !rest
                    .to_ascii_uppercase()
                    .contains("HEADER.FIELDS (MESSAGE-ID)")
                {
                    let _ = write.write_all(format!("{tag} BAD unsupported fetch\r\n").as_bytes());
                    continue;
                }
                let range = rest.split_whitespace().next().unwrap_or("").to_string();
                let count = mailbox.messages.len() as u32;
                for (index, message) in mailbox.messages.iter().enumerate() {
                    let seq = index as u32 + 1;
                    if !uid_in_range(seq, &range, count) {
                        continue;
                    }
                    let header = message_id_header(&message.raw);
                    let _ = write.write_all(
                        format!(
                            "* {seq} FETCH (UID {} BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n",
                            message.uid,
                            header.len()
                        )
                        .as_bytes(),
                    );
                    let _ = write.write_all(&header);
                    let _ = write.write_all(b")\r\n");
                }
                let _ = write.write_all(format!("{tag} OK FETCH\r\n").as_bytes());
            }
            "UID" => {
                if state.lock().expect("state").close_during_uid_fetch {
                    break;
                }
                let guard = state.lock().expect("state");
                if rest.to_ascii_uppercase().contains("CHANGEDSINCE")
                    && guard.condstore
                    && !condstore_enabled
                    && !selected_condstore
                {
                    let _ = write.write_all(
                        format!("{tag} BAD CHANGEDSINCE requires CONDSTORE\r\n").as_bytes(),
                    );
                    continue;
                }
                let Some((range, changed_since, mailbox_name)) = parse_uid_fetch(&rest, &guard)
                else {
                    let _ = write.write_all(format!("{tag} BAD uid fetch\r\n").as_bytes());
                    continue;
                };
                let Some(mailbox) = guard
                    .mailboxes
                    .iter()
                    .find(|item| item.name == mailbox_name)
                else {
                    let _ = write.write_all(format!("{tag} NO no mailbox\r\n").as_bytes());
                    continue;
                };
                if mailbox.reject_fetch {
                    let _ = write.write_all(format!("{tag} BAD UID FETCH denied\r\n").as_bytes());
                    continue;
                }
                if let Some(since) = changed_since
                    && qresync_enabled
                    && rest.to_ascii_uppercase().contains("VANISHED")
                {
                    let gone: Vec<String> = match &guard.vanished_reply {
                        Some(set) => vec![set.clone()],
                        None => mailbox
                            .vanished
                            .iter()
                            .filter(|(_, modseq)| *modseq > since)
                            .map(|(uid, _)| uid.to_string())
                            .collect(),
                    };
                    if !gone.is_empty() {
                        let _ = write.write_all(
                            format!("* VANISHED (EARLIER) {}\r\n", gone.join(",")).as_bytes(),
                        );
                    }
                }
                let mut bodies_left = guard.close_after_body_fetches;
                let highest = mailbox
                    .messages
                    .iter()
                    .map(|message| message.uid)
                    .max()
                    .unwrap_or(0);
                for (index, message) in mailbox.messages.iter().enumerate() {
                    if !uid_in_range(message.uid, &range, highest) {
                        continue;
                    }
                    if let Some(since) = changed_since
                        && message.modseq <= since
                    {
                        continue;
                    }
                    let seq = index + 1;
                    let upper_cmd = rest.to_ascii_uppercase();
                    if !upper_cmd.contains("BODY") {
                        if upper_cmd.contains("FLAGS") {
                            let flags = message.flags.join(" ");
                            let _ = write.write_all(
                                format!("* {seq} FETCH (UID {} FLAGS ({flags}))\r\n", message.uid)
                                    .as_bytes(),
                            );
                        } else {
                            let _ = write.write_all(
                                format!("* {seq} FETCH (UID {})\r\n", message.uid).as_bytes(),
                            );
                        }
                        continue;
                    }
                    let flags = message.flags.join(" ");
                    match message.body_form {
                        BodyForm::Literal => {
                            let header = format!(
                                "* {seq} FETCH (UID {} FLAGS ({flags}) BODY[] {{{}}}\r\n",
                                message.uid,
                                message.raw.len()
                            );
                            let _ = write.write_all(header.as_bytes());
                            let _ = write.write_all(&message.raw);
                            let _ = write.write_all(b")\r\n");
                        }
                        BodyForm::Quoted => {
                            let quoted = quote_bytes(&message.raw);
                            let _ = write.write_all(
                                format!(
                                    "* {seq} FETCH (UID {} FLAGS ({flags}) BODY[] {quoted})\r\n",
                                    message.uid
                                )
                                .as_bytes(),
                            );
                        }
                        BodyForm::Nil => {
                            let _ = write.write_all(
                                format!(
                                    "* {seq} FETCH (UID {} FLAGS ({flags}) BODY[] NIL)\r\n",
                                    message.uid
                                )
                                .as_bytes(),
                            );
                        }
                        BodyForm::Absent => {
                            let _ = write.write_all(
                                format!("* {seq} FETCH (UID {} FLAGS ({flags}))\r\n", message.uid)
                                    .as_bytes(),
                            );
                        }
                    }
                    if let Some(left) = bodies_left.as_mut() {
                        *left = left.saturating_sub(1);
                        if *left == 0 {
                            let _ = write.flush();
                            return;
                        }
                    }
                }
                let _ = write.write_all(format!("{tag} OK UID FETCH\r\n").as_bytes());
            }
            "LOGOUT" => {
                let _ = write.write_all(format!("* BYE\r\n{tag} OK LOGOUT\r\n").as_bytes());
                let _ = write.flush();
                break;
            }
            "NOOP" => {
                let _ = write.write_all(format!("{tag} OK NOOP\r\n").as_bytes());
            }
            "IDLE" if state.lock().expect("state").idle => {
                let (news_first, bad) = {
                    let guard = state.lock().expect("state");
                    (guard.idle_news_first, guard.idle_bad)
                };
                if news_first {
                    let _ = write.write_all(b"* 9 EXISTS\r\n+ idling\r\n");
                } else {
                    let _ = write.write_all(b"+ idling\r\n");
                }
                let _ = write.flush();
                if bad {
                    let _ = write.write_all(format!("{tag} BAD idle over\r\n").as_bytes());
                    let _ = write.flush();
                    continue;
                }
                let _ = read
                    .get_ref()
                    .set_read_timeout(Some(Duration::from_millis(20)));
                let mut pending = String::new();
                loop {
                    {
                        let mut guard = state.lock().expect("state");
                        if guard.idle_push {
                            guard.idle_push = false;
                            let exists = guard
                                .mailboxes
                                .iter()
                                .find(|mailbox| mailbox.name == "INBOX")
                                .map_or(0, |mailbox| mailbox.messages.len());
                            let _ = write.write_all(format!("* {exists} EXISTS\r\n").as_bytes());
                            let _ = write.flush();
                        }
                    }
                    match read.read_line(&mut pending) {
                        Ok(0) => return,
                        Ok(_) if pending.trim().eq_ignore_ascii_case("DONE") => break,
                        Ok(_) => pending.clear(),
                        Err(err)
                            if matches!(
                                err.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => return,
                    }
                }
                state.lock().expect("state").commands.push("DONE".into());
                let _ = read
                    .get_ref()
                    .set_read_timeout(Some(Duration::from_secs(5)));
                let _ = write.write_all(format!("{tag} OK IDLE terminated\r\n").as_bytes());
            }
            _ => {
                let _ = write.write_all(format!("{tag} BAD unsupported\r\n").as_bytes());
            }
        }
        let _ = write.flush();
    }
}

fn parse_uid_fetch(rest: &str, state: &TestState) -> Option<(String, Option<u64>, String)> {
    let rest = rest.trim();
    let rest = strip_prefix_ignore(rest, "FETCH")?.trim_start();
    let (range, after) = rest.split_once(' ').unwrap_or((rest, ""));
    let changed = after
        .to_ascii_uppercase()
        .split("CHANGEDSINCE")
        .nth(1)
        .and_then(|tail| {
            let digits: String = tail
                .trim_start()
                .trim_start_matches('(')
                .chars()
                .skip_while(|ch| ch.is_ascii_whitespace())
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            digits.parse().ok()
        });
    let mailbox = last_examined(&state.commands)?;
    Some((range.to_string(), changed, mailbox))
}

fn last_examined(commands: &[String]) -> Option<String> {
    for command in commands.iter().rev() {
        let upper = command.to_ascii_uppercase();
        let Some(index) = upper.find(" EXAMINE ") else {
            continue;
        };
        return Some(mailbox_name(command[index + " EXAMINE ".len()..].trim()));
    }
    None
}

/// RFC 3501: `n:*` includes the mailbox's highest UID even when `n` is greater
/// than every assigned UID.
fn uid_in_range(uid: u32, range: &str, highest: u32) -> bool {
    range
        .split(',')
        .any(|part| uid_in_one_range(uid, part, highest))
}

fn uid_in_one_range(uid: u32, range: &str, highest: u32) -> bool {
    let (start, end) = range.split_once(':').unwrap_or((range, range));
    let start = if start == "*" {
        u32::MAX
    } else {
        start.parse().unwrap_or(0)
    };
    let end_star = end == "*";
    let end = if end_star {
        u32::MAX
    } else {
        end.parse().unwrap_or(0)
    };
    if end_star && highest > 0 && start > highest && uid == highest {
        return true;
    }
    let (low, high) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    uid >= low && uid <= high
}

fn capability_text(state: &TestState) -> String {
    let mut caps = String::from("IMAP4rev1");
    if state.condstore {
        caps.push_str(" CONDSTORE");
    }
    if state.utf8_accept {
        caps.push_str(" UTF8=ACCEPT");
    }
    if state.idle {
        caps.push_str(" IDLE");
    }
    if state.qresync {
        caps.push_str(" QRESYNC");
    }
    if state.special_use || state.reject_list_return {
        caps.push_str(" SPECIAL-USE");
    }
    caps
}

fn is_special_use(flag: &str) -> bool {
    [
        "\\All",
        "\\Archive",
        "\\Drafts",
        "\\Flagged",
        "\\Junk",
        "\\Sent",
        "\\Trash",
    ]
    .iter()
    .any(|known| known.eq_ignore_ascii_case(flag))
}

/// The `Message-ID` line of a raw message, as `HEADER.FIELDS` returns it.
fn message_id_header(raw: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::new();
    for line in text.split("\r\n") {
        if line.is_empty() {
            break;
        }
        if line.to_ascii_lowercase().starts_with("message-id:") {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("\r\n");
    out.into_bytes()
}

fn read_client_command(
    read: &mut BufReader<TcpStream>,
    write: &mut impl Write,
) -> Option<(String, Vec<Vec<u8>>)> {
    let mut line = String::new();
    let mut literals = Vec::new();
    loop {
        let mut chunk = String::new();
        match read.read_line(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        line.push_str(chunk.trim_end_matches(['\r', '\n']));
        let Some((len, needs_continuation)) = trailing_literal(&line) else {
            return Some((line, literals));
        };
        if let Some(start) = line.rfind('{') {
            line.truncate(start);
        }
        if needs_continuation
            && (write.write_all(b"+ continue\r\n").is_err() || write.flush().is_err())
        {
            return None;
        }
        let mut buf = vec![0u8; len];
        if read.read_exact(&mut buf).is_err() {
            return None;
        }
        line.push_str(&String::from_utf8_lossy(&buf));
        literals.push(buf);
    }
}

fn trailing_literal(line: &str) -> Option<(usize, bool)> {
    if !line.ends_with('}') {
        return None;
    }
    let start = line.rfind('{')?;
    let mut inner = &line[start + 1..line.len() - 1];
    let nonsync = inner.ends_with('+');
    if nonsync {
        inner = &inner[..inner.len() - 1];
    }
    if inner.is_empty() || !inner.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((inner.parse().ok()?, !nonsync))
}

fn split_command(line: &str) -> Option<(String, String, String)> {
    let line = line.trim();
    let (tag, rest) = line.split_once(' ')?;
    let rest = rest.trim_start();
    let (verb, args) = rest.split_once(' ').unwrap_or((rest, ""));
    Some((tag.to_string(), verb.to_string(), args.to_string()))
}

fn login_args(rest: &str) -> (String, String) {
    let (user, rest) = take_astring(rest.trim());
    let (pass, _) = take_astring(rest.trim());
    (user, pass)
}

fn take_astring(input: &str) -> (String, &str) {
    if let Some(rest) = input.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
                continue;
            }
            if ch == '"' {
                let leftover = chars.as_str();
                return (out, leftover);
            }
            out.push(ch);
        }
        return (out, "");
    }
    let end = input.find(' ').unwrap_or(input.len());
    (input[..end].to_string(), &input[end..])
}

fn mailbox_needs_literal(name: &str) -> bool {
    name.bytes()
        .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
}

fn quote_bytes(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    for byte in bytes {
        match byte {
            b'\\' | b'"' => {
                out.push('\\');
                out.push(*byte as char);
            }
            byte if byte.is_ascii() && *byte != b'\r' && *byte != b'\n' => out.push(*byte as char),
            _ => out.push('?'),
        }
    }
    out.push('"');
    out
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn mailbox_name(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(quoted) = rest.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = quoted.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    name.push(next);
                }
                continue;
            }
            if ch == '"' {
                return name;
            }
            name.push(ch);
        }
        return name;
    }
    rest.split_whitespace().next().unwrap_or("").to_string()
}

fn strip_prefix_ignore<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

pub(super) fn raw_message(
    id: &str,
    in_reply_to: Option<&str>,
    subject: &str,
    from: &str,
    body: &str,
    extra_headers: &str,
) -> Vec<u8> {
    let mut text = format!(
        "From: {from}\r\nSubject: {subject}\r\nMessage-ID: <{id}>\r\nDate: Mon, 16 Nov 2026 12:00:00 +0000\r\n"
    );
    if let Some(parent) = in_reply_to {
        text.push_str(&format!(
            "In-Reply-To: <{parent}>\r\nReferences: <{parent}>\r\n"
        ));
    }
    text.push_str(extra_headers);
    text.push_str("MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n");
    text.push_str(body);
    text.into_bytes()
}
