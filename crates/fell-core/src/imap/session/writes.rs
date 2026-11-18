//! The write-back session: `SELECT` and the commands that move mail.
//!
//! Sync never holds one of these. It reads with a plain [`Session`], whose
//! commands are all read-only (`EXAMINE`, `BODY.PEEK`). A [`WriteSession`]
//! is opened only to replay the outbox, on its own connection, and moves
//! mail with `UID MOVE` (RFC 6851), or `UID COPY`, `UID STORE` of `\Deleted`,
//! and `UID EXPUNGE` (RFC 4315) when the server offers UIDPLUS but not MOVE.
//! It has no bare `EXPUNGE`: that would also remove mail another client
//! flagged `\Deleted`.

use crate::Error;

use super::super::ImapConfig;
use super::{Listed, Reply, Session, bracket_u32, quote_imap};

/// How a write command failed.
#[derive(Debug)]
pub(in crate::imap) enum WriteError {
    /// The server answered NO or BAD. The connection is still usable.
    Refused(String),
    /// The connection failed; nothing more can be sent on it.
    Connection(Error),
}

/// Where the server put the messages a move or copy took: the target's
/// UIDVALIDITY and `(source UID, new UID)` pairs, from `COPYUID`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::imap) struct Placed {
    pub uidvalidity: Option<u32>,
    pub uids: Vec<(u32, u32)>,
}

pub(in crate::imap) struct WriteSession {
    session: Session,
}

impl WriteSession {
    pub(in crate::imap) fn open(config: &ImapConfig) -> Result<Self, Error> {
        Ok(Self {
            session: Session::open(config)?,
        })
    }

    /// `UID MOVE` is offered.
    pub(in crate::imap) fn has_move(&self) -> bool {
        self.session.has_cap("MOVE")
    }

    /// UIDPLUS: `UID EXPUNGE` removes only the UIDs it names, and moves and
    /// copies report where the mail went.
    pub(in crate::imap) fn has_uidplus(&self) -> bool {
        self.session.has_cap("UIDPLUS")
    }

    pub(in crate::imap) fn list(&mut self) -> Result<Vec<Listed>, Error> {
        self.session.list_selectable()
    }

    /// `SELECT` the mailbox read-write and return its UIDVALIDITY.
    pub(in crate::imap) fn select(&mut self, mailbox: &str) -> Result<u32, WriteError> {
        let name = quote_imap(mailbox).map_err(|err| WriteError::Refused(err.to_string()))?;
        let reply = self.run("SELECT", &format!("SELECT {name}"))?;
        reply
            .untagged
            .iter()
            .find_map(|line| bracket_u32(&line.text, "UIDVALIDITY"))
            .ok_or_else(|| WriteError::Refused(format!("SELECT {mailbox} returned no UIDVALIDITY")))
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

    pub(in crate::imap) fn uid_copy(
        &mut self,
        uids: &[u32],
        target: &str,
    ) -> Result<Placed, WriteError> {
        let target = quote_imap(target).map_err(|err| WriteError::Refused(err.to_string()))?;
        let reply = self.run("UID COPY", &format!("UID COPY {} {target}", uid_set(uids)))?;
        Ok(copyuid(&reply.ok).unwrap_or_default())
    }

    /// Flag the UIDs `\Deleted`, then `UID EXPUNGE` exactly those. Only for
    /// a server with UIDPLUS; the caller checks.
    pub(in crate::imap) fn uid_remove(&mut self, uids: &[u32]) -> Result<(), WriteError> {
        let set = uid_set(uids);
        self.run(
            "UID STORE",
            &format!("UID STORE {set} +FLAGS.SILENT (\\Deleted)"),
        )?;
        self.run("UID EXPUNGE", &format!("UID EXPUNGE {set}"))?;
        Ok(())
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
}
