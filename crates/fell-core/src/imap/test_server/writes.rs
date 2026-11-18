//! The test server's write verbs, for the write-back outbox: `SELECT`,
//! `UID MOVE`, `UID COPY`, `UID STORE`, and `UID EXPUNGE`. They are off
//! unless a test turns them on in [`WriteFixture`]; sync tests never do.

use std::io::Write;
use std::sync::{Arc, Mutex};

use super::{Counted, FixtureMessage, TestState, mailbox_name, strip_prefix_ignore, uid_in_range};

/// What the server offers and how it answers writes.
#[derive(Debug, Default)]
pub(in crate::imap) struct WriteFixture {
    /// Advertise MOVE (RFC 6851).
    pub moves: bool,
    /// Advertise UIDPLUS (RFC 4315): COPYUID and `UID EXPUNGE`.
    pub uidplus: bool,
    /// Answer every `UID MOVE` and `UID COPY` with this NO text.
    pub refuse: Option<String>,
    /// Drop the connection after this many `UID MOVE` replies.
    pub close_after_moves: Option<u32>,
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
        "UID" => {
            let rest = rest.trim();
            let mut state = state.lock().expect("state");
            let mut writes = writes.lock().expect("writes");
            if let Some(args) = strip_prefix_ignore(rest, "MOVE ") {
                if !writes.moves {
                    format!("{tag} BAD MOVE not offered\r\n")
                } else {
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
                place(tag, args, &mut state, &writes, false)
            } else if let Some(args) = strip_prefix_ignore(rest, "STORE ") {
                store_deleted(tag, args, &mut state)
            } else if let Some(args) = strip_prefix_ignore(rest, "EXPUNGE ") {
                if !writes.uidplus {
                    format!("{tag} BAD UID EXPUNGE not offered\r\n")
                } else {
                    expunge(tag, args, &mut state)
                }
            } else {
                return false;
            }
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

/// `UID MOVE` (`remove`) or `UID COPY` of `<set> <mailbox>`.
fn place(
    tag: &str,
    args: &str,
    state: &mut TestState,
    writes: &WriteFixture,
    remove: bool,
) -> String {
    let verb = if remove { "MOVE" } else { "COPY" };
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
    if remove {
        if !copyuid.is_empty() {
            reply.push_str(&format!("* OK {copyuid}moved\r\n"));
        }
        reply.push_str(&take_out(state, from, &picked));
        reply.push_str(&format!("{tag} OK {verb} completed\r\n"));
    } else {
        reply.push_str(&format!("{tag} OK {copyuid}{verb} completed\r\n"));
    }
    reply
}

fn store_deleted(tag: &str, args: &str, state: &mut TestState) -> String {
    if !args
        .to_ascii_uppercase()
        .contains("+FLAGS.SILENT (\\DELETED)")
    {
        return format!("{tag} BAD unsupported store\r\n");
    }
    let Some(source) = selected(state) else {
        return format!("{tag} BAD no mailbox selected\r\n");
    };
    let set = args.split_whitespace().next().unwrap_or("").to_string();
    let Some(mailbox) = state.mailboxes.iter_mut().find(|item| item.name == source) else {
        return format!("{tag} NO no mailbox\r\n");
    };
    let highest = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    for message in &mut mailbox.messages {
        if uid_in_range(message.uid, &set, highest)
            && !message.flags.iter().any(|flag| flag == "\\Deleted")
        {
            message.flags.push("\\Deleted".into());
        }
    }
    format!("{tag} OK STORE completed\r\n")
}

fn expunge(tag: &str, args: &str, state: &mut TestState) -> String {
    let Some(source) = selected(state) else {
        return format!("{tag} BAD no mailbox selected\r\n");
    };
    let Some(from) = state.mailboxes.iter().position(|item| item.name == source) else {
        return format!("{tag} NO no mailbox\r\n");
    };
    let set = args.trim();
    let mailbox = &state.mailboxes[from];
    let highest = mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0);
    let picked: Vec<usize> = (0..mailbox.messages.len())
        .filter(|index| {
            let message = &mailbox.messages[*index];
            uid_in_range(message.uid, set, highest)
                && message.flags.iter().any(|flag| flag == "\\Deleted")
        })
        .collect();
    let mut reply = take_out(state, from, &picked);
    reply.push_str(&format!("{tag} OK EXPUNGE completed\r\n"));
    reply
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
