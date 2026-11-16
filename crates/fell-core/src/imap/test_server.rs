//! In-process IMAP server for tests. Speaks the read-only subset the client uses.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub(super) struct FixtureMessage {
    pub uid: u32,
    pub flags: Vec<String>,
    pub modseq: u64,
    pub raw: Vec<u8>,
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
    pub messages: Vec<FixtureMessage>,
}

pub(super) struct TestState {
    pub username: String,
    pub password: String,
    pub condstore: bool,
    pub close_after_starttls: bool,
    pub mailboxes: Vec<FixtureMailbox>,
    pub commands: Vec<String>,
}

pub(super) struct TestServer {
    pub addr: std::net::SocketAddr,
    pub state: Arc<Mutex<TestState>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    pub(super) fn spawn(state: TestState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(state));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = thread::spawn(move || accept_loop(listener, thread_state, thread_shutdown));
        Self {
            addr,
            state,
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

fn accept_loop(listener: TcpListener, state: Arc<Mutex<TestState>>, shutdown: Arc<AtomicBool>) {
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
                handle(stream, &state);
            }
            Err(_) => break,
        }
    }
}

fn handle(stream: TcpStream, state: &Arc<Mutex<TestState>>) {
    let mut write = stream.try_clone().expect("clone");
    let mut read = BufReader::new(stream);
    let _ = write.write_all(b"* OK fell-test ready\r\n");
    let _ = write.flush();
    let mut authed = false;
    let mut condstore_enabled = false;
    let mut selected_condstore = false;
    loop {
        let mut line = String::new();
        match read.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
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
                guard.commands.push("LOGIN".into());
            } else {
                guard.commands.push(line.clone());
            }
        }
        let verb_upper = verb.to_ascii_uppercase();
        match verb_upper.as_str() {
            "CAPABILITY" => {
                let caps = if state.lock().expect("state").condstore {
                    "IMAP4rev1 CONDSTORE"
                } else {
                    "IMAP4rev1"
                };
                let _ = write.write_all(
                    format!("* CAPABILITY {caps}\r\n{tag} OK CAPABILITY\r\n").as_bytes(),
                );
            }
            "ENABLE" => {
                if !authed {
                    let _ = write.write_all(
                        format!("{tag} NO ENABLE is only valid in the authenticated state\r\n")
                            .as_bytes(),
                    );
                } else {
                    condstore_enabled = true;
                    let _ = write.write_all(
                        format!("* ENABLED CONDSTORE\r\n{tag} OK ENABLE\r\n").as_bytes(),
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
                let (user, pass) = login_args(&rest);
                let guard = state.lock().expect("state");
                if user == guard.username && pass == guard.password {
                    authed = true;
                    let caps = if guard.condstore {
                        "IMAP4rev1 CONDSTORE"
                    } else {
                        "IMAP4rev1"
                    };
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
                for mailbox in &guard.mailboxes {
                    let flags = if mailbox.list_flags.is_empty() {
                        "\\HasNoChildren".to_string()
                    } else {
                        mailbox.list_flags.clone()
                    };
                    let name = quote(&mailbox.name);
                    let _ =
                        write.write_all(format!("* LIST ({flags}) \"/\" {name}\r\n").as_bytes());
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
                        .max()
                        .unwrap_or(1);
                    let _ = write.write_all(
                        format!("* OK [HIGHESTMODSEQ {highest}] highest\r\n").as_bytes(),
                    );
                }
                let _ = write
                    .write_all(format!("{tag} OK [READ-ONLY] EXAMINE completed\r\n").as_bytes());
            }
            "UID" => {
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
                    let header = format!(
                        "* {seq} FETCH (UID {} FLAGS ({flags}) BODY[] {{{}}}\r\n",
                        message.uid,
                        message.raw.len()
                    );
                    let _ = write.write_all(header.as_bytes());
                    let _ = write.write_all(&message.raw);
                    let _ = write.write_all(b")\r\n");
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
