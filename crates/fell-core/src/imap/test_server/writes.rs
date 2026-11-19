//! The test server's write-back verbs: `SELECT` and `UID MOVE`, plus the
//! `STATUS`, `UID SEARCH HEADER Message-ID`, and Message-ID header fetch
//! replay uses to find a moved message. The selected write connection also
//! supports UID STORE for Seen and Flagged, with conditional conflicts.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{
    Counted, FixtureMessage, TestState, mailbox_name, message_id_header, strip_prefix_ignore,
    uid_in_range,
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
    /// Answer `UID SEARCH HEADER Message-ID` with NO.
    pub refuse_search: bool,
    /// Drop the connection after this many `UID MOVE` replies.
    pub close_after_moves: Option<u32>,
    /// Apply the next command with this verb (`"MOVE"`), then drop the
    /// connection before any reply.
    pub lose_reply_to: Option<&'static str>,
    pub before_store: Option<Box<dyn FnOnce() + Send>>,
    pub modified_no: bool,
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
        "SELECT" => select(
            tag,
            rest,
            &state.lock().expect("state"),
            writes.lock().expect("writes").nomodseq,
        ),
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
                && selected_write
            {
                fetch_message_ids(tag, args, &state)
            } else if let Some(args) = strip_prefix_ignore(rest, "STORE ") {
                if !selected_write {
                    format!("{tag} BAD STORE requires SELECT\r\n")
                } else {
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
            } else if ["COPY ", "EXPUNGE"]
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

fn select(tag: &str, rest: &str, state: &TestState, nomodseq: bool) -> String {
    let name = mailbox_name(rest);
    let Some(mailbox) = state.mailboxes.iter().find(|item| item.name == name) else {
        return format!("{tag} NO no such mailbox\r\n");
    };
    let highest = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    let next = mailbox.uidnext.unwrap_or(highest + 1);
    let revision = if !state.condstore {
        String::new()
    } else if nomodseq {
        "* OK [NOMODSEQ] no revisions\r\n".into()
    } else {
        let highest = mailbox.messages.iter().map(|m| m.modseq).max().unwrap_or(1);
        format!("* OK [HIGHESTMODSEQ {highest}] highest\r\n")
    };
    format!(
        "* {} EXISTS\r\n* OK [UIDVALIDITY {}] UIDs valid\r\n* OK [UIDNEXT {next}] next\r\n\
         {revision}{tag} OK [READ-WRITE] SELECT completed\r\n",
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
        let flags = message.flags.join(" ");
        let revision = if state.condstore && !writes.nomodseq {
            format!(" MODSEQ ({})", message.modseq)
        } else {
            String::new()
        };
        reply.push_str(&format!(
            "* {} FETCH (UID {} FLAGS ({flags}){revision})\r\n",
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
    let (add, flag) = match rest {
        "+FLAGS.SILENT (\\Seen)" => (true, "\\Seen"),
        "-FLAGS.SILENT (\\Seen)" => (false, "\\Seen"),
        "+FLAGS.SILENT (\\Flagged)" => (true, "\\Flagged"),
        "-FLAGS.SILENT (\\Flagged)" => (false, "\\Flagged"),
        _ => return format!("{tag} BAD expected one supported system flag\r\n"),
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
    if conditional {
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
