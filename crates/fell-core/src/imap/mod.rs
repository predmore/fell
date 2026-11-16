//! Read-only generic IMAP. Proton Mail Bridge is one account of this kind.
//!
//! The session lists mailboxes, examines them, and fetches with `BODY.PEEK`
//! so the server is not asked to mark mail read. There is no SMTP, no OAuth,
//! and no send command. IDLE and removal of vanished messages are later work.

mod rfc822;
mod session;
mod utf7;

#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod test_server;

use std::collections::HashMap;
use std::fmt;

use crate::Error;
use crate::account::AccountId;
use crate::backend::{FlagUpdate, MailboxDelta, SyncCursor};
use crate::message::RemoteId;

pub(crate) fn fetch_imap(
    account_id: &AccountId,
    config: &ImapConfig,
    cursor: Option<&SyncCursor>,
) -> Result<MailboxDelta, Error> {
    if config.username.is_empty() {
        return Err(Error::Backend("imap username is empty".into()));
    }
    let mut session = session::Session::open(config)?;
    let mut mailboxes = session.list_selectable()?;
    mailboxes.sort_by(|left, right| {
        inbox_rank(&left.decoded)
            .cmp(&inbox_rank(&right.decoded))
            .then_with(|| left.decoded.cmp(&right.decoded))
            .then_with(|| left.raw.cmp(&right.raw))
    });
    let prior: HashMap<String, MailboxCheckpoint> = decode_cursor(cursor).into_iter().collect();
    let mut messages = Vec::new();
    let mut flags = Vec::new();
    let mut checkpoints = Vec::new();
    for mailbox in &mailboxes {
        let previous = prior.get(&mailbox.raw);
        let Some(examined) = session.examine(&mailbox.raw)? else {
            if let Some(prev) = previous {
                checkpoints.push((mailbox.raw.clone(), prev.clone()));
            }
            continue;
        };
        let plan = plan_fetch(previous, &examined);
        let same_validity = previous.is_some_and(|prev| prev.uidvalidity == examined.uidvalidity);
        // High-water body fetches ignore UIDs already at the checkpoint.
        // CHANGEDSINCE results are kept so an older flag update is applied.
        let seen_through = match (previous, &plan) {
            (Some(prev), FetchPlan::HighWater { .. } | FetchPlan::Discover { .. })
                if same_validity =>
            {
                Some(prev.uid)
            }
            _ => None,
        };
        let mut fetched = match &plan {
            FetchPlan::ChangedSince(modseq) => session.uid_fetch("1:*", Some(*modseq))?,
            FetchPlan::HighWater { start } => session.uid_fetch(&format!("{start}:*"), None)?,
            FetchPlan::Full => session.uid_fetch("1:*", None)?,
            FetchPlan::Discover { start } => {
                let found = session.uid_find(&format!("{start}:*"))?;
                let mut bodies = Vec::new();
                for uid in found.into_iter().filter(|uid| *uid >= *start) {
                    bodies.extend(session.uid_fetch(&uid.to_string(), None)?);
                }
                bodies
            }
            FetchPlan::Idle => Vec::new(),
        };
        let highest = fetched.iter().map(|item| item.uid).max();
        fetched.retain(|item| seen_through.is_none_or(|uid| item.uid > uid));
        let needs_baseline = same_validity
            && previous.is_some_and(|prev| prev.modseq.is_none() && prev.uid > 0)
            && examined.highest_modseq.is_some()
            && !matches!(plan, FetchPlan::Full | FetchPlan::ChangedSince(_));
        if needs_baseline {
            for (uid, flag_names) in session.uid_flags("1:*")? {
                let unread = !flag_names
                    .iter()
                    .any(|flag| flag.eq_ignore_ascii_case("\\Seen"));
                flags.push(FlagUpdate {
                    remote_id: RemoteId(format!(
                        "imap:{}:{uid}:{}",
                        examined.uidvalidity,
                        escape_mailbox(&mailbox.raw)
                    )),
                    unread,
                });
            }
        }
        for item in fetched {
            if item.skipped || rfc822::is_deleted(&item.flags) {
                continue;
            }
            messages.push(rfc822::to_message(
                account_id,
                &mailbox.raw,
                &mailbox.decoded,
                examined.uidvalidity,
                item.uid,
                &item.flags,
                &item.body,
            ));
        }
        let uid = match &plan {
            FetchPlan::Full | FetchPlan::HighWater { .. } => match examined.uidnext {
                Some(next) => highest.unwrap_or(0).max(next.saturating_sub(1)),
                None => checkpoint_uid(previous, same_validity, highest),
            },
            FetchPlan::Discover { .. } | FetchPlan::ChangedSince(_) | FetchPlan::Idle => {
                checkpoint_uid(previous, same_validity, highest)
            }
        };
        // Record HIGHESTMODSEQ when this pass saw every message's flags: a full
        // or CHANGEDSINCE fetch, an empty mailbox, a high-water pass from uid 0,
        // or a UID FLAGS baseline for older mail that had no modseq yet.
        let saw_every_flag = matches!(plan, FetchPlan::Full | FetchPlan::ChangedSince(_))
            || examined.exists == 0
            || needs_baseline
            || (matches!(plan, FetchPlan::HighWater { start } if start <= 1)
                && previous.map(|prev| prev.uid).unwrap_or(0) == 0);
        let modseq = if saw_every_flag {
            examined
                .highest_modseq
                .or_else(|| previous.and_then(|prev| prev.modseq))
        } else if same_validity {
            previous.and_then(|prev| prev.modseq)
        } else {
            None
        };
        checkpoints.push((
            mailbox.raw.clone(),
            MailboxCheckpoint {
                uidvalidity: examined.uidvalidity,
                uid,
                modseq,
            },
        ));
    }
    session.logout();
    checkpoints.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(MailboxDelta {
        messages,
        flags,
        cursor: Some(encode_cursor(account_id.as_str(), &checkpoints)),
    })
}

fn inbox_rank(name: &str) -> u8 {
    if name.eq_ignore_ascii_case("INBOX") {
        1
    } else {
        0
    }
}

enum FetchPlan {
    /// Nothing new, or the mailbox is empty.
    Idle,
    /// Every message, including flags.
    Full,
    /// CONDSTORE flag and new-message pass.
    ChangedSince(u64),
    /// Body fetch from `start`. UIDNEXT is known, so the checkpoint can move
    /// to `UIDNEXT - 1` after this succeeds.
    HighWater { start: u32 },
    /// UIDNEXT was omitted. Learn UIDs without bodies, then fetch only new ones.
    Discover { start: u32 },
}

fn checkpoint_uid(
    previous: Option<&MailboxCheckpoint>,
    same_validity: bool,
    highest: Option<u32>,
) -> u32 {
    let prior = if same_validity {
        previous.map(|prev| prev.uid).unwrap_or(0)
    } else {
        0
    };
    prior.max(highest.unwrap_or(0))
}

fn plan_fetch(previous: Option<&MailboxCheckpoint>, examined: &session::Examined) -> FetchPlan {
    if examined.exists == 0 {
        return FetchPlan::Idle;
    }
    match previous {
        Some(prev) if prev.uidvalidity == examined.uidvalidity => {
            if let Some(modseq) = prev.modseq
                && examined.highest_modseq.is_some()
            {
                return FetchPlan::ChangedSince(modseq);
            }
            if prev.uid == u32::MAX
                || examined
                    .uidnext
                    .is_some_and(|next| next <= prev.uid.saturating_add(1))
            {
                return FetchPlan::Idle;
            }
            let start = prev.uid.saturating_add(1);
            if examined.uidnext.is_some() {
                FetchPlan::HighWater { start }
            } else {
                FetchPlan::Discover { start }
            }
        }
        _ => FetchPlan::Full,
    }
}

/// How the TCP session is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapSecurity {
    /// Cleartext. Local tests, and a Bridge install set to plain IMAP.
    Plain,
    /// Upgrade with STARTTLS after the greeting. Bridge's usual local port.
    StartTls,
    /// Implicit TLS, typically port 993.
    Tls,
}

/// Connection settings for one IMAP account.
///
/// The password is redacted in [`Debug`] output.
#[derive(Clone, PartialEq, Eq)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub security: ImapSecurity,
    /// Bridge's local certificate is not a public CA. Leave this false for
    /// any host that is not the user's own machine.
    pub accept_invalid_certs: bool,
}

impl fmt::Debug for ImapConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("security", &self.security)
            .field("accept_invalid_certs", &self.accept_invalid_certs)
            .finish()
    }
}

impl ImapConfig {
    /// Proton Mail Bridge defaults: local STARTTLS, certificate not public.
    pub fn bridge_local(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1143,
            username: username.into(),
            password: password.into(),
            security: ImapSecurity::StartTls,
            accept_invalid_certs: true,
        }
    }
}

/// Per-mailbox checkpoint. `uid` is the highest UID already stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxCheckpoint {
    pub uidvalidity: u32,
    pub uid: u32,
    pub modseq: Option<u64>,
}

/// Decode a sync cursor written by [`encode_cursor`].
///
/// An empty token and any token this crate did not write are "no checkpoint",
/// so the next sync fetches every mailbox from UID 1.
pub fn decode_cursor(cursor: Option<&SyncCursor>) -> Vec<(String, MailboxCheckpoint)> {
    let Some(cursor) = cursor else {
        return Vec::new();
    };
    let mut lines = cursor.token.lines();
    if lines.next() != Some("fell-imap/1") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let Some(name) = parts.next().and_then(unescape_mailbox) else {
            return Vec::new();
        };
        let Some(uidvalidity) = parts.next().and_then(|raw| raw.parse().ok()) else {
            return Vec::new();
        };
        let Some(uid) = parts.next().and_then(|raw| raw.parse().ok()) else {
            return Vec::new();
        };
        let modseq = match parts.next() {
            Some("-") => None,
            Some(raw) => match raw.parse().ok() {
                Some(value) => Some(value),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        };
        if parts.next().is_some() {
            return Vec::new();
        }
        out.push((
            name,
            MailboxCheckpoint {
                uidvalidity,
                uid,
                modseq,
            },
        ));
    }
    out
}

pub fn encode_cursor(account_key: &str, mailboxes: &[(String, MailboxCheckpoint)]) -> SyncCursor {
    let mut token = String::from("fell-imap/1");
    for (name, checkpoint) in mailboxes {
        let modseq = checkpoint
            .modseq
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into());
        token.push('\n');
        token.push_str(&escape_mailbox(name));
        token.push('\t');
        token.push_str(&checkpoint.uidvalidity.to_string());
        token.push('\t');
        token.push_str(&checkpoint.uid.to_string());
        token.push('\t');
        token.push_str(&modseq);
    }
    SyncCursor {
        account_key: account_key.to_string(),
        token,
    }
}

pub(super) fn escape_mailbox(name: &str) -> String {
    name.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape_mailbox(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_redacted_in_debug() {
        let config = ImapConfig::bridge_local("caleb", "secret-bridge");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("secret-bridge"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("1143"));
    }

    #[test]
    fn cursor_round_trips_mailbox_names() {
        let mailboxes = vec![
            (
                "INBOX".into(),
                MailboxCheckpoint {
                    uidvalidity: 9,
                    uid: 40,
                    modseq: Some(12),
                },
            ),
            (
                "Sent\tItems".into(),
                MailboxCheckpoint {
                    uidvalidity: 3,
                    uid: 0,
                    modseq: None,
                },
            ),
        ];
        let cursor = encode_cursor("acct", &mailboxes);
        assert_eq!(cursor.account_key, "acct");
        assert_eq!(decode_cursor(Some(&cursor)), mailboxes);
    }

    #[test]
    fn unknown_cursor_starts_over() {
        let cursor = SyncCursor {
            account_key: "acct".into(),
            token: "not-ours".into(),
        };
        assert!(decode_cursor(Some(&cursor)).is_empty());
        assert!(decode_cursor(None).is_empty());
    }
}
