//! The write-back session: `SELECT`, `UID MOVE` and single-flag `UID STORE`.
//!
//! Sync never holds one of these. It reads with a plain [`Session`], whose
//! commands are all read-only (`EXAMINE`, `BODY.PEEK`). A [`WriteSession`]
//! is opened only to replay the outbox, on its own connection. A server
//! without MOVE can still receive read and star writes.

use crate::Error;
use crate::outbox::OutboxKind;
use crate::store::copies::CopyFlags;

use super::super::ImapConfig;
use super::{
    Listed, Reply, Session, bracket_u32, fetch_modseq, number_after, parse_fetch, parse_flags,
    quote_imap, search_uids,
};

/// How a write command failed.
#[derive(Debug)]
pub(in crate::imap) enum WriteError {
    /// The server answered NO or BAD. The connection is still usable.
    Refused(String),
    /// The connection failed; nothing more can be sent on it.
    Connection(Error),
}

/// Where the server put the messages a move took: the target's UIDVALIDITY
/// and `(source UID, new UID)` pairs, from `COPYUID`. A UID with no pair
/// was not placed by this command, or the server did not say (no UIDPLUS,
/// or nothing matched because an earlier try already moved it); replay
/// then looks for the message in the target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::imap) struct Placed {
    pub uidvalidity: Option<u32>,
    pub uids: Vec<(u32, u32)>,
}

pub(in crate::imap) struct WriteSession {
    session: Session,
    conditional: bool,
}

pub(in crate::imap) enum FlagReply {
    Stored(Option<u64>),
    Modified,
}

impl WriteSession {
    pub(in crate::imap) fn open(config: &ImapConfig) -> Result<Self, Error> {
        Ok(Self {
            session: Session::open(config)?,
            conditional: false,
        })
    }

    /// `UID MOVE` is offered.
    pub(in crate::imap) fn has_move(&self) -> bool {
        self.session.has_cap("MOVE")
    }

    pub(in crate::imap) fn list(&mut self) -> Result<Vec<Listed>, Error> {
        self.session.list_selectable()
    }

    /// `SELECT` the mailbox read-write and return its UIDVALIDITY.
    pub(in crate::imap) fn select(&mut self, mailbox: &str) -> Result<u32, WriteError> {
        let name = quote_imap(mailbox).map_err(|err| WriteError::Refused(err.to_string()))?;
        let modifier = if self.session.condstore {
            " (CONDSTORE)"
        } else {
            ""
        };
        let reply = self.run("SELECT", &format!("SELECT {name}{modifier}"))?;
        self.conditional = self.session.condstore
            && !reply
                .untagged
                .iter()
                .any(|line| has_code(&line.text, "NOMODSEQ"));
        reply
            .untagged
            .iter()
            .find_map(|line| bracket_u32(&line.text, "UIDVALIDITY"))
            .ok_or_else(|| WriteError::Refused(format!("SELECT {mailbox} returned no UIDVALIDITY")))
    }

    pub(in crate::imap) fn conditional(&self) -> bool {
        self.conditional
    }

    /// The selected copy's current flags and revision, if the UID still exists.
    pub(in crate::imap) fn uid_copy_flags(
        &mut self,
        uid: u32,
    ) -> Result<Option<CopyFlags>, WriteError> {
        let items = if self.conditional {
            "UID FLAGS MODSEQ"
        } else {
            "UID FLAGS"
        };
        let reply = self.run("UID FETCH", &format!("UID FETCH {uid} ({items})"))?;
        Ok(reply
            .untagged
            .iter()
            .filter_map(|line| {
                if let Some(item) = parse_flags(&line.text) {
                    let has = |flag: &str| {
                        item.flags
                            .iter()
                            .any(|value| value.eq_ignore_ascii_case(flag))
                    };
                    return (item.uid == uid).then(|| CopyFlags {
                        modseq: item.modseq,
                        seen: Some(has("\\Seen")),
                        flagged: Some(has("\\Flagged")),
                    });
                }
                let upper = line.text.to_ascii_uppercase();
                (super::uid_in_fetch_text(&upper) == Some(uid)).then(|| CopyFlags {
                    modseq: fetch_modseq(&upper),
                    seen: None,
                    flagged: None,
                })
            })
            .max_by_key(|flags| (flags.modseq, flags.seen.is_some())))
    }

    pub(in crate::imap) fn uid_store_flag(
        &mut self,
        uid: u32,
        kind: OutboxKind,
        value: bool,
        modseq: Option<u64>,
    ) -> Result<FlagReply, WriteError> {
        let flag = kind
            .flag_name()
            .ok_or_else(|| WriteError::Refused("expected a system flag".into()))?;
        let modifier = modseq
            .map(|modseq| format!(" (UNCHANGEDSINCE {modseq})"))
            .unwrap_or_default();
        let sign = if value { '+' } else { '-' };
        let reply = match self.run(
            "UID STORE",
            &format!("UID STORE {uid}{modifier} {sign}FLAGS.SILENT ({flag})"),
        ) {
            Ok(reply) => reply,
            Err(WriteError::Refused(text))
                if modseq.is_some()
                    && text
                        .strip_prefix("imap: UID STORE failed: ")
                        .is_some_and(|status| has_code(status, "MODIFIED")) =>
            {
                return Ok(FlagReply::Modified);
            }
            Err(err) => return Err(err),
        };
        if modseq.is_some() && has_code(&reply.ok, "MODIFIED") {
            return Ok(FlagReply::Modified);
        }
        let revision = reply.untagged.iter().find_map(|line| {
            let upper = line.text.to_ascii_uppercase();
            (super::uid_in_fetch_text(&upper) == Some(uid))
                .then(|| fetch_modseq(&upper))
                .flatten()
        });
        Ok(FlagReply::Stored(revision))
    }

    /// `UID MOVE` the UIDs into `target`. UIDs the mailbox no longer has are
    /// skipped by the server, not an error.
    pub(in crate::imap) fn uid_move(
        &mut self,
        uids: &[u32],
        target: &str,
    ) -> Result<Placed, WriteError> {
        let target = quote_imap(target).map_err(|err| WriteError::Refused(err.to_string()))?;
        let reply = self.run("UID MOVE", &format!("UID MOVE {} {target}", uid_set(uids)))?;
        // RFC 6851 sends COPYUID in an untagged OK before the expunges.
        Ok(reply
            .untagged
            .iter()
            .map(|line| line.text.as_str())
            .chain(std::iter::once(reply.ok.as_str()))
            .find_map(copyuid)
            .unwrap_or_default())
    }

    /// `STATUS` of a mailbox that is not selected: its UIDVALIDITY and
    /// UIDNEXT. Every message that arrives there later has a UID at least
    /// UIDNEXT while UIDVALIDITY stays the same.
    pub(in crate::imap) fn status(&mut self, mailbox: &str) -> Result<(u32, u32), WriteError> {
        let name = quote_imap(mailbox).map_err(|err| WriteError::Refused(err.to_string()))?;
        let reply = self.run("STATUS", &format!("STATUS {name} (UIDVALIDITY UIDNEXT)"))?;
        reply
            .untagged
            .iter()
            .find_map(|line| status_values(&line.text))
            .ok_or_else(|| {
                WriteError::Refused(format!(
                    "STATUS {mailbox} returned no UIDVALIDITY or UIDNEXT"
                ))
            })
    }

    /// UIDs in the selected mailbox whose `Message-ID` header names
    /// `message_id` (stored without its angle brackets).
    pub(in crate::imap) fn search_message_id(
        &mut self,
        message_id: &str,
    ) -> Result<Vec<u32>, WriteError> {
        let value = quote_imap(&format!("<{message_id}>"))
            .map_err(|err| WriteError::Refused(err.to_string()))?;
        let reply = self.run(
            "UID SEARCH",
            &format!("UID SEARCH HEADER Message-ID {value}"),
        )?;
        Ok(search_uids(&reply.untagged))
    }

    /// The `Message-ID` header of each of the UIDs in the selected mailbox,
    /// as `(uid, header bytes)`. Headers only, with `BODY.PEEK`, so nothing
    /// is marked read. A UID the mailbox does not have is left out.
    pub(in crate::imap) fn uid_fetch_message_ids(
        &mut self,
        uids: &[u32],
    ) -> Result<Vec<(u32, Vec<u8>)>, WriteError> {
        let reply = self.run(
            "UID FETCH",
            &format!(
                "UID FETCH {} (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])",
                uid_set(uids)
            ),
        )?;
        Ok(reply
            .untagged
            .into_iter()
            .filter_map(|line| parse_fetch(&line.text, line.literals))
            .map(|fetched| (fetched.uid, fetched.body))
            .collect())
    }

    pub(in crate::imap) fn logout(&mut self) {
        self.session.logout();
    }

    fn run(&mut self, label: &str, command: &str) -> Result<Reply, WriteError> {
        match self.session.command(label, command) {
            Ok(reply) => Ok(reply),
            Err(Error::Backend(message)) if message.contains(&format!("{label} failed:")) => {
                Err(WriteError::Refused(message))
            }
            Err(err) => Err(WriteError::Connection(err)),
        }
    }
}

/// Match only the response code immediately after the status atom.
fn has_code(text: &str, name: &str) -> bool {
    let mut words = text.split_ascii_whitespace();
    let Some(mut status) = words.next() else {
        return false;
    };
    if status == "*" {
        status = words.next().unwrap_or_default();
    }
    ["OK", "NO"]
        .iter()
        .any(|valid| status.eq_ignore_ascii_case(valid))
        && words
            .next()
            .and_then(|word| word.strip_prefix('['))
            .is_some_and(|word| word.trim_end_matches(']').eq_ignore_ascii_case(name))
}

/// `1,3,5:7` for the UIDs, ascending.
fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = String::new();
    let mut index = 0;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end + 1 {
            index += 1;
            end = sorted[index];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}:{end}"));
        }
        index += 1;
    }
    out
}

/// `* STATUS <mailbox> (UIDVALIDITY <n> UIDNEXT <n>)`: both values. Read
/// from the last parenthesis, so a mailbox name cannot stand in for them.
fn status_values(text: &str) -> Option<(u32, u32)> {
    let upper = text.trim_start().to_ascii_uppercase();
    let rest = upper
        .strip_prefix('*')?
        .trim_start()
        .strip_prefix("STATUS ")?;
    let items = &rest[rest.rfind('(')?..];
    Some((
        number_after(items, "UIDVALIDITY ")?,
        number_after(items, "UIDNEXT ")?,
    ))
}

/// `[COPYUID <uidvalidity> <source set> <new set>]` anywhere in a reply line.
fn copyuid(text: &str) -> Option<Placed> {
    let upper = text.to_ascii_uppercase();
    let at = upper.find("[COPYUID ")?;
    let rest = &text[at + "[COPYUID ".len()..];
    let rest = &rest[..rest.find(']')?];
    let mut words = rest.split_whitespace();
    let uidvalidity: u32 = words.next()?.parse().ok()?;
    let from = expand(words.next()?)?;
    let to = expand(words.next()?)?;
    if from.len() != to.len() {
        return None;
    }
    Some(Placed {
        uidvalidity: Some(uidvalidity),
        uids: from.into_iter().zip(to).collect(),
    })
}

/// Every UID of a set, in the order the set names them. A range runs in the
/// order written (RFC 4315 sets list source and target in the same order).
fn expand(set: &str) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    for part in set.split(',') {
        let number = |raw: &str| raw.parse::<u32>().ok().filter(|uid| *uid > 0);
        match part.split_once(':') {
            Some((low, high)) => {
                let (low, high) = (number(low)?, number(high)?);
                if low.abs_diff(high) > 100_000 {
                    return None;
                }
                if low <= high {
                    out.extend(low..=high);
                } else {
                    out.extend((high..=low).rev());
                }
            }
            None => out.push(number(part)?),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_sets_join_runs() {
        assert_eq!(uid_set(&[7, 1, 2, 3, 5, 5]), "1:3,5,7");
        assert_eq!(uid_set(&[4]), "4");
    }

    #[test]
    fn copyuid_pairs_source_and_target_in_order() {
        let placed = copyuid("OK [COPYUID 38505 304,319:320 3956:3958] Done").unwrap();
        assert_eq!(placed.uidvalidity, Some(38505));
        assert_eq!(placed.uids, vec![(304, 3956), (319, 3957), (320, 3958)]);
        assert_eq!(copyuid("* OK [COPYUID 9 1:2 5] short"), None);
        assert_eq!(copyuid("OK MOVE completed"), None);
    }

    #[test]
    fn status_reads_both_values_after_the_mailbox_name() {
        assert_eq!(
            status_values("* STATUS \"Archive\" (UIDVALIDITY 30 UIDNEXT 7)"),
            Some((30, 7))
        );
        assert_eq!(
            status_values("* STATUS \"UIDNEXT 9 (x\" (UIDNEXT 4 UIDVALIDITY 2)"),
            Some((2, 4))
        );
        assert_eq!(status_values("* STATUS \"Archive\" (UIDVALIDITY 30)"), None);
        assert_eq!(status_values("* OK [UIDNEXT 3] next"), None);
    }
}
