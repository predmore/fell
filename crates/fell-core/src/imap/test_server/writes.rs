//! The test server's write-back verbs: `SELECT` and `UID MOVE`, plus the
//! `STATUS`, `UID SEARCH HEADER Message-ID`, and Message-ID header fetch
//! replay uses to find a moved message. `UID MOVE` is off unless a test
//! turns it on in [`WriteFixture`]; sync tests never do. There is no
//! `COPY`, `STORE`, or `EXPUNGE`: write-back never sends them, and a test
//! that sees one in the command log fails.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{
    Counted, FixtureMessage, TestState, mailbox_name, message_id_header, strip_prefix_ignore,
    uid_in_range,
};

/// What the server offers and how it answers writes.
#[derive(Debug, Default)]
pub(in crate::imap) struct WriteFixture {
    /// Advertise MOVE (RFC 6851).
    pub moves: bool,
    /// Advertise UIDPLUS (RFC 4315): `UID MOVE` reports COPYUID.
    pub uidplus: bool,
    /// Answer every `UID MOVE` with this NO text.
    pub refuse: Option<String>,
    /// Answer `UID SEARCH HEADER Message-ID` with NO.
    pub refuse_search: bool,
    /// Drop the connection after this many `UID MOVE` replies.
    pub close_after_moves: Option<u32>,
    /// Apply the next command with this verb (`"MOVE"`), then drop the
    /// connection before any reply.
    pub lose_reply_to: Option<&'static str>,
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
    out: &mut Counted,
) -> bool {
    let reply = match verb {
        "SELECT" => select(tag, rest, &state.lock().expect("state")),
        "STATUS" => status(tag, rest, &state.lock().expect("state")),
        "UID" => {
            let rest = rest.trim();
            let mut state = state.lock().expect("state");
            let mut writes = writes.lock().expect("writes");
            let lose = writes
                .lose_reply_to
                .is_some_and(|verb| strip_prefix_ignore(rest, &format!("{verb} ")).is_some());
            let reply = if let Some(args) = strip_prefix_ignore(rest, "SEARCH HEADER Message-ID ") {
                if writes.refuse_search {
                    format!("{tag} NO search not now\r\n")
                } else {
                    search_message_id(tag, args, &state)
                }
            } else if let Some(args) = strip_prefix_ignore(rest, "FETCH ")
                && args
                    .to_ascii_uppercase()
                    .contains("BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)]")
                && selected(&state).is_some()
            {
                fetch_message_ids(tag, args, &state)
            } else if let Some(args) = strip_prefix_ignore(rest, "MOVE ") {
                if !writes.moves {
                    format!("{tag} BAD MOVE not offered\r\n")
                } else {
                    let reply = place(tag, args, &mut state, &writes);
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
            } else if ["COPY ", "STORE ", "EXPUNGE"]
                .iter()
                .any(|verb| strip_prefix_ignore(rest, verb).is_some())
            {
                format!("{tag} BAD write-back never sends this\r\n")
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

fn select(tag: &str, rest: &str, state: &TestState) -> String {
    let name = mailbox_name(rest);
    let Some(mailbox) = state.mailboxes.iter().find(|item| item.name == name) else {
        return format!("{tag} NO no such mailbox\r\n");
    };
    let highest = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    let next = mailbox.uidnext.unwrap_or(highest + 1);
    format!(
        "* {} EXISTS\r\n* OK [UIDVALIDITY {}] UIDs valid\r\n* OK [UIDNEXT {next}] next\r\n\
         {tag} OK [READ-WRITE] SELECT completed\r\n",
        mailbox.messages.len(),
        mailbox.uidvalidity
    )
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
    format!(
        "* STATUS {name:?} (UIDVALIDITY {} UIDNEXT {next})\r\n{tag} OK STATUS completed\r\n",
        mailbox.uidvalidity
    )
}

/// `UID FETCH <set> (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])` in the
/// selected mailbox.
fn fetch_message_ids(tag: &str, args: &str, state: &TestState) -> String {
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

/// `UID MOVE <set> <mailbox>`.
fn place(tag: &str, args: &str, state: &mut TestState, writes: &WriteFixture) -> String {
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
    if !copyuid.is_empty() {
        reply.push_str(&format!("* OK {copyuid}moved\r\n"));
    }
    reply.push_str(&take_out(state, from, &picked));
    reply.push_str(&format!("{tag} OK MOVE completed\r\n"));
    reply
}

/// `UID SEARCH HEADER Message-ID "<id>"` in the selected mailbox. As RFC
/// 3501 says, a match is any header value that holds the string, ignoring
/// case, so replay must check what it finds.
fn search_message_id(tag: &str, args: &str, state: &TestState) -> String {
    let Some(source) = selected(state) else {
        return format!("{tag} BAD no mailbox selected\r\n");
    };
    let Some(mailbox) = state.mailboxes.iter().find(|item| item.name == source) else {
        return format!("{tag} NO no mailbox\r\n");
    };
    let wanted = mailbox_name(args);
    let mut line = String::from("* SEARCH");
    for message in &mailbox.messages {
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
