//! The test server's write-back verbs: `SELECT` and `UID MOVE`, plus the
//! `STATUS`, `UID SEARCH HEADER Message-ID`, and Message-ID header fetch
//! replay uses to find a moved message. The selected write connection also
//! supports UID STORE for Seen and Flagged, with conditional conflicts.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{
    Counted, FixtureMailbox, FixtureMessage, TestState, mailbox_name, message_id_header,
    strip_prefix_ignore, uid_in_range,
};

/// What the server offers and how it answers writes.
#[derive(Default)]
pub(in crate::imap) struct WriteFixture {
    /// Advertise MOVE (RFC 6851).
    pub moves: bool,
    /// Advertise UIDPLUS (RFC 4315): `UID MOVE` reports COPYUID.
    pub uidplus: bool,
    /// Answer every `UID MOVE` with this NO text.
    pub refuse: Option<String>,
    /// Run the hook when the next `SELECT` arrives, then answer it with NO.
    pub before_refusing_select: Option<Box<dyn FnOnce() + Send>>,
    /// Answer `UID SEARCH HEADER Message-ID` with NO.
    pub refuse_search: bool,
    /// Return this many tagged-OK empty HEADER searches before finding the copy.
    pub empty_header_searches: usize,
    /// Override a selected write session's bounded search with this many UID hits.
    pub range_search_hits: Option<usize>,
    pub omit_message_id_header: Option<u32>,
    pub close_on_login: bool,
    pub refuse_append: bool,
    /// Disconnect without committing the next APPEND.
    pub lose_append_before_commit: bool,
    /// Drop the connection after this many `UID MOVE` replies.
    pub close_after_moves: Option<u32>,
    /// Disconnect without applying the next `UID MOVE`.
    pub lose_before_move: bool,
    /// Apply the next command with this verb (`"MOVE"`), then drop the
    /// connection before any reply.
    pub lose_reply_to: Option<&'static str>,
    pub lose_before_store: bool,
    pub before_store: Option<Box<dyn FnOnce() + Send>>,
    pub refuse_store_uid: Option<u32>,
    pub read_only_mailbox: Option<String>,
    pub modified_no: bool,
    pub omit_store_modseq: bool,
    pub omit_fetch_flags: bool,
    pub nomodseq: bool,
}

pub(super) fn caps(writes: &Mutex<WriteFixture>) -> String {
    let writes = writes.lock().expect("writes");
    let mut caps = String::new();
    if writes.moves {
        caps.push_str(" MOVE");
    }
    if writes.uidplus {
        caps.push_str(" UIDPLUS");
    }
    caps
}

/// Answer `verb` when it is a write. `false` leaves it to the read-only
/// handlers.
pub(super) fn handle(
    tag: &str,
    verb: &str,
    rest: &str,
    state: &Arc<Mutex<TestState>>,
    writes: &Arc<Mutex<WriteFixture>>,
    selected_write: bool,
    out: &mut Counted,
) -> bool {
    let reply = match verb {
        "SELECT" => {
            let (hook, nomodseq, read_only) = {
                let mut writes = writes.lock().expect("writes");
                (
                    writes.before_refusing_select.take(),
                    writes.nomodseq,
                    writes.read_only_mailbox.as_deref() == Some(mailbox_name(rest).as_str()),
                )
            };
            if let Some(hook) = hook {
                hook();
                format!("{tag} NO SELECT refused\r\n")
            } else {
                select(
                    tag,
                    rest,
                    &state.lock().expect("state"),
                    nomodseq,
                    read_only,
                )
            }
        }
        "STATUS" => status(tag, rest, &state.lock().expect("state")),
        "CREATE" => create_label(tag, rest, &mut state.lock().expect("state")),
        "UID" => {
            let rest = rest.trim();
            let mut state = state.lock().expect("state");
            let mut writes = writes.lock().expect("writes");
            let lose = writes
                .lose_reply_to
                .is_some_and(|verb| strip_prefix_ignore(rest, &format!("{verb} ")).is_some());
            let reply = if let Some(args) = strip_prefix_ignore(rest, "SEARCH HEADER Message-ID ") {
                let range = args
                    .rsplit_once(" UID ")
                    .map(|(_, range)| range)
                    .filter(|range| !range.ends_with('"'));
                if writes.refuse_search {
                    format!("{tag} NO search not now\r\n")
                } else if writes.empty_header_searches > 0 {
                    writes.empty_header_searches -= 1;
                    format!("* SEARCH\r\n{tag} OK SEARCH\r\n")
                } else if range.is_some()
                    && let Some(hits) = writes.range_search_hits
                {
                    format!("* SEARCH{}\r\n{tag} OK SEARCH\r\n", " 1".repeat(hits))
                } else {
                    search_message_id(tag, args, range, &state)
                }
            } else if selected_write
                && strip_prefix_ignore(rest, "SEARCH UID ").is_some()
                && let Some(hits) = writes.range_search_hits
            {
                format!("* SEARCH{}\r\n{tag} OK SEARCH\r\n", " 1".repeat(hits))
            } else if let Some(args) = strip_prefix_ignore(rest, "FETCH ")
                && args
                    .to_ascii_uppercase()
                    .contains("BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)]")
                && selected_write
            {
                fetch_message_ids(tag, args, &state, writes.omit_message_id_header)
            } else if let Some(args) = strip_prefix_ignore(rest, "STORE ") {
                if !selected_write {
                    format!("{tag} BAD STORE requires SELECT\r\n")
                } else if writes
                    .read_only_mailbox
                    .as_deref()
                    .is_some_and(|mailbox| selected(&state).as_deref() == Some(mailbox))
                {
                    format!("{tag} NO [READ-ONLY] STORE refused\r\n")
                } else {
                    if std::mem::take(&mut writes.lose_before_store) {
                        let _ = out.stream.shutdown(std::net::Shutdown::Both);
                        return true;
                    }
                    if let Some(hook) = writes.before_store.take() {
                        hook();
                    }
                    store_flag(tag, args, &mut state, &writes)
                }
            } else if let Some(args) = strip_prefix_ignore(rest, "FETCH ")
                && selected_write
                && args.to_ascii_uppercase().contains("UID FLAGS")
            {
                fetch_flag(tag, args, &state, &writes)
            } else if let Some(args) = strip_prefix_ignore(rest, "MOVE ") {
                if !writes.moves {
                    format!("{tag} BAD MOVE not offered\r\n")
                } else {
                    if std::mem::take(&mut writes.lose_before_move) {
                        let _ = out.stream.shutdown(std::net::Shutdown::Both);
                        return true;
                    }
                    let reply = place(tag, args, &mut state, &writes, true);
                    if let Some(left) = writes.close_after_moves.as_mut() {
                        *left = left.saturating_sub(1);
                        if *left == 0 {
                            writes.close_after_moves = None;
                            let _ = out.write_all(reply.as_bytes());
                            let _ = out.flush();
                            let _ = out.stream.shutdown(std::net::Shutdown::Both);
                            return true;
                        }
                    }
                    reply
                }
            } else if let Some(args) = strip_prefix_ignore(rest, "COPY ") {
                if !selected_write {
                    format!("{tag} BAD COPY requires SELECT\r\n")
                } else {
                    place(tag, args, &mut state, &writes, false)
                }
            } else if let Some(args) = strip_prefix_ignore(rest, "EXPUNGE ") {
                if !selected_write || !writes.uidplus {
                    format!("{tag} BAD UID EXPUNGE unavailable\r\n")
                } else {
                    expunge_uid(tag, args, &mut state)
                }
            } else {
                return false;
            };
            if lose {
                writes.lose_reply_to = None;
                let _ = out.stream.shutdown(std::net::Shutdown::Both);
                return true;
            }
            reply
        }
        "EXPUNGE" | "STORE" | "COPY" | "MOVE" => format!("{tag} BAD use UID commands\r\n"),
        _ => return false,
    };
    let _ = out.write_all(reply.as_bytes());
    let _ = out.flush();
    true
}

/// The mailbox the last SELECT named.
fn selected(state: &TestState) -> Option<String> {
    state.commands.iter().rev().find_map(|command| {
        let upper = command.to_ascii_uppercase();
        let index = upper.find(" SELECT ")?;
        Some(mailbox_name(&command[index + " SELECT ".len()..]))
    })
}

pub(super) fn append(
    tag: &str,
    rest: &str,
    mut literals: Vec<Vec<u8>>,
    state: &Mutex<TestState>,
    writes: &Mutex<WriteFixture>,
    out: &mut Counted,
) {
    let mut state = state.lock().expect("state");
    let mut writes = writes.lock().expect("writes");
    if std::mem::take(&mut writes.lose_append_before_commit) {
        let _ = out.stream.shutdown(std::net::Shutdown::Both);
        return;
    }
    let name = mailbox_name(rest);
    let mailbox = state.mailboxes.iter_mut().find(|item| item.name == name);
    let reply = match (writes.refuse_append, mailbox, literals.pop()) {
        (true, _, _) => format!("{tag} NO Sent storage temporarily unavailable\r\n"),
        (false, Some(mailbox), Some(raw)) => {
            let uid = mailbox.uidnext.unwrap_or_else(|| {
                mailbox
                    .messages
                    .iter()
                    .map(|message| message.uid)
                    .max()
                    .unwrap_or(0)
                    + 1
            });
            mailbox.uidnext = Some(uid + 1);
            let flags = rest
                .rsplit_once(" (")
                .and_then(|(_, flags)| flags.strip_suffix(')'))
                .map(|flags| flags.split_ascii_whitespace().map(str::to_string).collect())
                .unwrap_or_default();
            mailbox.messages.push(FixtureMessage {
                uid,
                flags,
                modseq: 1,
                raw,
                body_form: super::BodyForm::Literal,
            });
            if writes.lose_reply_to == Some("APPEND") {
                writes.lose_reply_to = None;
                let _ = out.stream.shutdown(std::net::Shutdown::Both);
                return;
            }
            if writes.uidplus {
                format!(
                    "{tag} OK [APPENDUID {} {uid}] APPEND done\r\n",
                    mailbox.uidvalidity
                )
            } else {
                format!("{tag} OK APPEND done\r\n")
            }
        }
        _ => format!("{tag} BAD invalid APPEND\r\n"),
    };
    let _ = out.write_all(reply.as_bytes());
    let _ = out.flush();
}

fn select(tag: &str, rest: &str, state: &TestState, nomodseq: bool, read_only: bool) -> String {
    let name = mailbox_name(rest);
    let Some(mailbox) = state.mailboxes.iter().find(|item| item.name == name) else {
        return format!("{tag} NO no such mailbox\r\n");
    };
    let highest = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    let next = mailbox.uidnext.unwrap_or(highest + 1);
    let next = if mailbox.omit_uidnext {
        String::new()
    } else {
        format!("* OK [UIDNEXT {next}] next\r\n")
    };
    let revision = if !state.condstore {
        String::new()
    } else if nomodseq {
        "* OK [NOMODSEQ] no revisions\r\n".into()
    } else {
        let highest = mailbox.messages.iter().map(|m| m.modseq).max().unwrap_or(1);
        format!("* OK [HIGHESTMODSEQ {highest}] highest\r\n")
    };
    let access = if read_only { "READ-ONLY" } else { "READ-WRITE" };
    format!(
        "* {} EXISTS\r\n* OK [UIDVALIDITY {}] UIDs valid\r\n{next}\
         {revision}{tag} OK [{access}] SELECT completed\r\n",
        mailbox.messages.len(),
        mailbox.uidvalidity
    )
}

fn fetch_flag(tag: &str, args: &str, state: &TestState, writes: &WriteFixture) -> String {
    let uid = args
        .split_ascii_whitespace()
        .next()
        .and_then(|value| value.parse::<u32>().ok());
    let mut reply = String::new();
    if let Some(mailbox) =
        selected(state).and_then(|name| state.mailboxes.iter().find(|m| m.name == name))
        && let Some((index, message)) = mailbox
            .messages
            .iter()
            .enumerate()
            .find(|(_, m)| Some(m.uid) == uid)
    {
        let flags = if writes.omit_fetch_flags {
            String::new()
        } else {
            format!(" FLAGS ({})", message.flags.join(" "))
        };
        let revision = if state.condstore && !writes.nomodseq {
            format!(" MODSEQ ({})", message.modseq)
        } else {
            String::new()
        };
        reply.push_str(&format!(
            "* {} FETCH (UID {}{flags}{revision})\r\n",
            index + 1,
            message.uid
        ));
    }
    reply.push_str(&format!("{tag} OK FETCH\r\n"));
    reply
}

fn store_flag(tag: &str, args: &str, state: &mut TestState, writes: &WriteFixture) -> String {
    let Some((uid, rest)) = args.split_once(' ') else {
        return format!("{tag} BAD expected UID\r\n");
    };
    let Ok(uid) = uid.parse::<u32>() else {
        return format!("{tag} BAD expected UID\r\n");
    };
    if writes.refuse_store_uid == Some(uid) {
        return format!("{tag} NO STORE refused\r\n");
    }
    let conditional = state.condstore && !writes.nomodseq;
    let (expected, rest) = if let Some(modifier) = rest.strip_prefix("(UNCHANGEDSINCE ") {
        let Some((revision, rest)) = modifier.split_once(") ") else {
            return format!("{tag} BAD invalid UNCHANGEDSINCE\r\n");
        };
        let Ok(revision) = revision.parse::<u64>() else {
            return format!("{tag} BAD invalid MODSEQ\r\n");
        };
        if !conditional {
            return format!("{tag} BAD conditional STORE unavailable\r\n");
        }
        (Some(revision), rest)
    } else {
        (None, rest)
    };
    let (add, flag) = match (
        rest.strip_prefix("+FLAGS.SILENT ("),
        rest.strip_prefix("-FLAGS.SILENT ("),
    ) {
        (Some(flag), _) => (true, flag),
        (_, Some(flag)) => (false, flag),
        _ => return format!("{tag} BAD expected incremental STORE\r\n"),
    };
    let Some(flag) = flag.strip_suffix(')').filter(|flag| !flag.contains(' ')) else {
        return format!("{tag} BAD expected one flag\r\n");
    };
    let Some(mailbox) =
        selected(state).and_then(|name| state.mailboxes.iter_mut().find(|m| m.name == name))
    else {
        return format!("{tag} BAD no selected mailbox\r\n");
    };
    let revision = mailbox.messages.iter().map(|m| m.modseq).max().unwrap_or(0) + 1;
    let Some((index, message)) = mailbox
        .messages
        .iter_mut()
        .enumerate()
        .find(|(_, m)| m.uid == uid)
    else {
        return format!("{tag} OK STORE\r\n");
    };
    if expected.is_some_and(|expected| message.modseq > expected) {
        let status = if writes.modified_no { "NO" } else { "OK" };
        return format!("{tag} {status} [MODIFIED {uid}] conditional STORE failed\r\n");
    }
    if add {
        if !message
            .flags
            .iter()
            .any(|current| current.eq_ignore_ascii_case(flag))
        {
            message.flags.push(flag.into());
        }
    } else {
        message
            .flags
            .retain(|current| !current.eq_ignore_ascii_case(flag));
    }
    message.modseq = revision;
    if conditional && !writes.omit_store_modseq {
        format!(
            "* {} FETCH (UID {uid} MODSEQ ({revision}))\r\n{tag} OK STORE\r\n",
            index + 1
        )
    } else {
        format!("{tag} OK STORE\r\n")
    }
}

/// `STATUS <mailbox> (UIDVALIDITY UIDNEXT)`.
fn status(tag: &str, rest: &str, state: &TestState) -> String {
    let name = rest.rsplit_once(" (").map_or(rest, |(name, _)| name);
    let name = mailbox_name(name);
    let Some(mailbox) = state.mailboxes.iter().find(|item| item.name == name) else {
        return format!("{tag} NO no such mailbox\r\n");
    };
    let highest = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    let next = mailbox.uidnext.unwrap_or(highest + 1).max(highest + 1);
    let next = if mailbox.omit_uidnext {
        String::new()
    } else {
        format!(" UIDNEXT {next}")
    };
    format!(
        "* STATUS {name:?} (UIDVALIDITY {}{next})\r\n{tag} OK STATUS completed\r\n",
        mailbox.uidvalidity
    )
}

/// `UID FETCH <set> (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])` in the
/// selected mailbox.
fn fetch_message_ids(tag: &str, args: &str, state: &TestState, omit_header: Option<u32>) -> String {
    let Some(name) = selected(state) else {
        return format!("{tag} BAD no mailbox selected\r\n");
    };
    let Some(mailbox) = state.mailboxes.iter().find(|item| item.name == name) else {
        return format!("{tag} NO no mailbox\r\n");
    };
    let set = args.split_whitespace().next().unwrap_or("");
    let highest = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    let mut reply = String::new();
    for (index, message) in mailbox.messages.iter().enumerate() {
        if !uid_in_range(message.uid, set, highest) {
            continue;
        }
        if omit_header == Some(message.uid) {
            reply.push_str(&format!("* {} FETCH (UID {})\r\n", index + 1, message.uid));
            continue;
        }
        let header = String::from_utf8_lossy(&message_id_header(&message.raw)).into_owned();
        reply.push_str(&format!(
            "* {} FETCH (UID {} BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n{header})\r\n",
            index + 1,
            message.uid,
            header.len()
        ));
    }
    reply.push_str(&format!("{tag} OK FETCH completed\r\n"));
    reply
}

/// `UID MOVE` or `UID COPY` of a set into a target mailbox.
fn place(
    tag: &str,
    args: &str,
    state: &mut TestState,
    writes: &WriteFixture,
    moving: bool,
) -> String {
    if let Some(text) = &writes.refuse {
        return format!("{tag} NO {text}\r\n");
    }
    let Some(source) = selected(state) else {
        return format!("{tag} BAD no mailbox selected\r\n");
    };
    let (set, target) = args.trim().split_once(' ').unwrap_or((args, ""));
    let target = mailbox_name(target);
    let Some(to) = state.mailboxes.iter().position(|item| item.name == target) else {
        return format!("{tag} NO [TRYCREATE] no such mailbox\r\n");
    };
    let Some(from) = state.mailboxes.iter().position(|item| item.name == source) else {
        return format!("{tag} NO no mailbox\r\n");
    };
    let highest = state.mailboxes[from]
        .messages
        .iter()
        .map(|m| m.uid)
        .max()
        .unwrap_or(0);
    let picked: Vec<usize> = (0..state.mailboxes[from].messages.len())
        .filter(|index| uid_in_range(state.mailboxes[from].messages[*index].uid, set, highest))
        .collect();
    let mut pairs = Vec::new();
    for index in &picked {
        let message = &state.mailboxes[from].messages[*index];
        let copy = FixtureMessage {
            uid: 0,
            flags: message
                .flags
                .iter()
                .filter(|flag| !flag.eq_ignore_ascii_case("\\Deleted"))
                .cloned()
                .collect(),
            modseq: 0,
            raw: message.raw.clone(),
            body_form: message.body_form,
        };
        let source_uid = message.uid;
        let target = &mut state.mailboxes[to];
        let next = target
            .messages
            .iter()
            .map(|m| m.uid + 1)
            .chain(target.uidnext)
            .max()
            .unwrap_or(1);
        let modseq = target.messages.iter().map(|m| m.modseq).max().unwrap_or(0) + 1;
        target.messages.push(FixtureMessage {
            uid: next,
            modseq,
            ..copy
        });
        if target.uidnext.is_some() {
            target.uidnext = Some(next + 1);
        }
        pairs.push((source_uid, next));
    }
    let mut reply = String::new();
    let copyuid = if writes.uidplus && !pairs.is_empty() {
        let join = |pick: fn(&(u32, u32)) -> u32| {
            pairs
                .iter()
                .map(|pair| pick(pair).to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "[COPYUID {} {} {}] ",
            state.mailboxes[to].uidvalidity,
            join(|pair| pair.0),
            join(|pair| pair.1)
        )
    } else {
        String::new()
    };
    if moving {
        if !copyuid.is_empty() {
            reply.push_str(&format!("* OK {copyuid}moved\r\n"));
        }
        reply.push_str(&take_out(state, from, &picked));
        reply.push_str(&format!("{tag} OK MOVE completed\r\n"));
    } else {
        reply.push_str(&format!("{tag} OK {copyuid}COPY completed\r\n"));
    }
    reply
}

fn create_label(tag: &str, rest: &str, state: &mut TestState) -> String {
    let name = mailbox_name(rest);
    if !name.starts_with("Labels/") || state.mailboxes.iter().any(|m| m.name == name) {
        return format!("{tag} NO cannot create mailbox\r\n");
    }
    let validity = state
        .mailboxes
        .iter()
        .map(|m| m.uidvalidity)
        .max()
        .unwrap_or(0)
        + 1;
    state.mailboxes.push(FixtureMailbox {
        name,
        uidvalidity: validity,
        uidnext: Some(1),
        list_flags: String::new(),
        omit_uidnext: false,
        reject_examine: false,
        reject_fetch: false,
        vanished: Vec::new(),
        messages: Vec::new(),
    });
    format!("{tag} OK CREATE completed\r\n")
}

fn expunge_uid(tag: &str, args: &str, state: &mut TestState) -> String {
    let Some(name) = selected(state) else {
        return format!("{tag} BAD no mailbox selected\r\n");
    };
    let Some(index) = state
        .mailboxes
        .iter()
        .position(|mailbox| mailbox.name == name)
    else {
        return format!("{tag} NO mailbox is gone\r\n");
    };
    let messages = &state.mailboxes[index].messages;
    let highest = messages
        .iter()
        .map(|message| message.uid)
        .max()
        .unwrap_or(0);
    let picked: Vec<_> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            uid_in_range(message.uid, args, highest)
                && message
                    .flags
                    .iter()
                    .any(|flag| flag.eq_ignore_ascii_case("\\Deleted"))
        })
        .map(|(index, _)| index)
        .collect();
    let mut reply = take_out(state, index, &picked);
    reply.push_str(&format!("{tag} OK UID EXPUNGE completed\r\n"));
    reply
}

/// `UID SEARCH HEADER Message-ID "<id>"` in the selected mailbox. As RFC
/// 3501 says, a match is any header value that holds the string, ignoring
/// case, so replay must check what it finds.
fn search_message_id(tag: &str, args: &str, range: Option<&str>, state: &TestState) -> String {
    let Some(source) = selected(state) else {
        return format!("{tag} BAD no mailbox selected\r\n");
    };
    let Some(mailbox) = state.mailboxes.iter().find(|item| item.name == source) else {
        return format!("{tag} NO no mailbox\r\n");
    };
    let wanted = mailbox_name(args);
    let mut line = String::from("* SEARCH");
    for message in &mailbox.messages {
        if range.is_some_and(|range| !uid_in_range(message.uid, range, 0)) {
            continue;
        }
        let raw = String::from_utf8_lossy(&message.raw);
        let found = raw
            .split("\r\n")
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .any(|(name, value)| {
                name.eq_ignore_ascii_case("Message-ID")
                    && value
                        .to_ascii_lowercase()
                        .contains(&wanted.to_ascii_lowercase())
            });
        if found {
            line.push_str(&format!(" {}", message.uid));
        }
    }
    format!("{line}\r\n{tag} OK SEARCH completed\r\n")
}

/// Remove the messages at `picked` (ascending indexes) and announce each.
/// UIDNEXT stays past the UIDs that left, as on a real server.
fn take_out(state: &mut TestState, from: usize, picked: &[usize]) -> String {
    let mailbox = &mut state.mailboxes[from];
    let highest = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    let next = mailbox.uidnext.unwrap_or(highest + 1);
    let mut reply = String::new();
    for index in picked.iter().rev() {
        let message = mailbox.messages.remove(*index);
        let modseq = mailbox.messages.iter().map(|m| m.modseq).max().unwrap_or(0) + 1;
        mailbox.vanished.push((message.uid, modseq));
        reply.push_str(&format!("* {} EXPUNGE\r\n", index + 1));
    }
    if !picked.is_empty() {
        mailbox.uidnext = Some(next);
    }
    reply
}
