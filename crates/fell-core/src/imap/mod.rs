//! Read-only generic IMAP. Proton Mail Bridge is one account of this kind.
//!
//! The session lists mailboxes, examines them, and fetches with `BODY.PEEK`
//! so the server is not asked to mark mail read. There is no SMTP, no OAuth,
//! and no send command. INBOX IDLE is read-only too. Removal of vanished
//! messages is later work.

mod rfc822;
mod session;
mod utf7;

pub(crate) use session::{TlsStream, is_loopback_host as host_is_loopback, wrap_tcp_as_tls};

#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod test_server;

use std::collections::HashMap;
use std::fmt;

use crate::Error;
use crate::account::AccountId;
use crate::backend::{FlagUpdate, MailboxDelta, SyncCursor};
use crate::message::{Message, RemoteId};

/// Messages held in memory before they are persisted. A larger mailbox is
/// written in several deltas so the cursor can move before the FETCH ends.
const BATCH_MESSAGES: usize = 32;
const BATCH_BYTES: usize = 8 * 1024 * 1024;

fn batch_message_limit() -> usize {
    #[cfg(test)]
    {
        TEST_BATCH_MESSAGES.with(|limit| limit.get())
    }
    #[cfg(not(test))]
    {
        BATCH_MESSAGES
    }
}

#[cfg(test)]
thread_local! {
    static TEST_BATCH_MESSAGES: std::cell::Cell<usize> = const { std::cell::Cell::new(BATCH_MESSAGES) };
}

#[cfg(test)]
pub(super) struct BatchLimitGuard;

#[cfg(test)]
pub(super) fn limit_fetch_batch(messages: usize) -> BatchLimitGuard {
    TEST_BATCH_MESSAGES.with(|cell| cell.set(messages));
    BatchLimitGuard
}

#[cfg(test)]
impl Drop for BatchLimitGuard {
    fn drop(&mut self) {
        TEST_BATCH_MESSAGES.with(|cell| cell.set(BATCH_MESSAGES));
    }
}

/// Log in, count selectable mailboxes, and log out. Reads no mail.
pub(crate) fn check_login(config: &ImapConfig) -> Result<usize, Error> {
    if config.username.is_empty() {
        return Err(Error::Backend("imap username is empty".into()));
    }
    let mut session = session::Session::open(config)?;
    let count = session.list_selectable()?.len();
    session.logout();
    Ok(count)
}

/// How [`wait_for_inbox`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxWait {
    /// INBOX changed since the cursor, or while waiting.
    Changed,
    /// `max` passed with no news.
    Quiet,
    /// The caller asked to stop.
    Stopped,
    /// The server has no IDLE.
    Unsupported,
}

/// Wait on INBOX with IDLE until it changes, `max` passes, or `stop` is true.
///
/// Read-only: `EXAMINE`, `IDLE`, `DONE`, `LOGOUT`. Mail that arrived after
/// the last sync (`cursor`) but before IDLE started counts as a change, so
/// nothing waits a whole interval. IDLE is renewed every 20 minutes,
/// under the 29 minutes RFC 2177 allows.
pub(crate) fn wait_for_inbox(
    config: &ImapConfig,
    cursor: Option<&SyncCursor>,
    max: std::time::Duration,
    stop: &mut dyn FnMut() -> bool,
) -> Result<InboxWait, Error> {
    const RENEW: std::time::Duration = std::time::Duration::from_secs(20 * 60);
    let mut session = session::Session::open(config)?;
    if !session.has_idle() {
        session.logout();
        return Ok(InboxWait::Unsupported);
    }
    let Some(examined) = session.examine("INBOX")? else {
        session.logout();
        return Ok(InboxWait::Unsupported);
    };
    let checkpoints = decode_cursor(cursor);
    let inbox = checkpoints
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("INBOX"))
        .map(|(_, checkpoint)| checkpoint);
    if inbox_moved_since(inbox, &examined) {
        session.logout();
        return Ok(InboxWait::Changed);
    }
    let started = std::time::Instant::now();
    let outcome = loop {
        let left = max.saturating_sub(started.elapsed());
        if left.is_zero() {
            break InboxWait::Quiet;
        }
        match session.idle(left.min(RENEW), stop)? {
            session::IdleEnd::Changed => break InboxWait::Changed,
            session::IdleEnd::Stopped => break InboxWait::Stopped,
            session::IdleEnd::Quiet => continue,
        }
    };
    session.logout();
    Ok(outcome)
}

/// INBOX has mail the last sync did not see.
fn inbox_moved_since(checkpoint: Option<&MailboxCheckpoint>, examined: &session::Examined) -> bool {
    let Some(checkpoint) = checkpoint else {
        return true;
    };
    if checkpoint.uidvalidity != examined.uidvalidity {
        return true;
    }
    if examined
        .uidnext
        .is_some_and(|next| next > checkpoint.uid.saturating_add(1))
    {
        return true;
    }
    matches!(
        (checkpoint.modseq, examined.highest_modseq),
        (Some(seen), Some(now)) if now > seen
    )
}

pub(crate) fn fetch_imap(
    account_id: &AccountId,
    config: &ImapConfig,
    cursor: Option<&SyncCursor>,
    mut persist: impl FnMut(MailboxDelta) -> Result<(), Error>,
) -> Result<(), Error> {
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
    let mut done: HashMap<String, MailboxCheckpoint> = HashMap::new();
    for mailbox in &mailboxes {
        let previous = prior.get(&mailbox.raw);
        let Some(examined) = session.examine(&mailbox.raw)? else {
            if let Some(prev) = previous {
                done.insert(mailbox.raw.clone(), prev.clone());
                emit_progress(
                    &mut persist,
                    &Snapshot {
                        account_id,
                        listed: &mailboxes,
                        prior: &prior,
                        done: &done,
                    },
                    None,
                    Vec::new(),
                    Vec::new(),
                )?;
            }
            continue;
        };
        let plan = plan_fetch(previous, &examined);
        let previous_owned = previous.cloned();
        let same_validity = previous_owned
            .as_ref()
            .is_some_and(|prev| prev.uidvalidity == examined.uidvalidity);
        // High-water body fetches ignore UIDs already at the checkpoint.
        // CHANGEDSINCE results are kept so an older flag update is applied.
        let seen_through = match (previous_owned.as_ref(), &plan) {
            (Some(prev), FetchPlan::HighWater { .. } | FetchPlan::Discover { .. })
                if same_validity =>
            {
                Some(prev.uid)
            }
            _ => None,
        };
        let uidvalidity = examined.uidvalidity;
        let uidnext = examined.uidnext;
        let highest_modseq = examined.highest_modseq;
        let exists = examined.exists;
        let mut batch = Vec::new();
        let mut batch_bytes = 0usize;
        let mut highest: Option<u32> = None;
        let mut min_unparsed: Option<u32> = None;
        let flush_batch = |batch: &mut Vec<Message>,
                           batch_bytes: &mut usize,
                           highest: Option<u32>,
                           persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error>,
                           done: &HashMap<String, MailboxCheckpoint>|
         -> Result<(), Error> {
            if batch.is_empty() {
                return Ok(());
            }
            let uid = checkpoint_uid(previous_owned.as_ref(), same_validity, highest);
            let modseq = if same_validity {
                previous_owned.as_ref().and_then(|prev| prev.modseq)
            } else {
                None
            };
            let current = MailboxCheckpoint {
                uidvalidity,
                uid,
                modseq,
            };
            emit_progress(
                persist,
                &Snapshot {
                    account_id,
                    listed: &mailboxes,
                    prior: &prior,
                    done,
                },
                Some((&mailbox.raw, &current)),
                std::mem::take(batch),
                Vec::new(),
            )?;
            *batch_bytes = 0;
            Ok(())
        };
        let stop = stream_bodies(&mut session, &plan, |item| {
            let counts = seen_through.is_none_or(|floor| item.uid > floor);
            if counts && item.body_missing {
                min_unparsed = Some(min_unparsed.map_or(item.uid, |uid| uid.min(item.uid)));
            } else if counts {
                highest = Some(highest.map_or(item.uid, |current| current.max(item.uid)));
            }
            if !counts || item.skipped || item.body_missing || rfc822::is_deleted(&item.flags) {
                return Ok(());
            }
            let raw_len = item.body.len();
            batch.push(rfc822::to_message(
                account_id,
                &mailbox.raw,
                &mailbox.decoded,
                uidvalidity,
                item.uid,
                &item.flags,
                &item.body,
            ));
            batch_bytes += raw_len;
            if batch.len() >= batch_message_limit() || batch_bytes >= BATCH_BYTES {
                flush_batch(
                    &mut batch,
                    &mut batch_bytes,
                    capped_highest(highest, min_unparsed),
                    &mut persist,
                    &done,
                )?;
            }
            Ok(())
        })?;
        let (finished, min_skipped) = match stop {
            session::FetchStop::Done => (true, None),
            session::FetchStop::Refused => {
                if batch.is_empty() && highest.is_none() {
                    if let Some(prev) = previous_owned.clone() {
                        done.insert(mailbox.raw.clone(), prev);
                        emit_progress(
                            &mut persist,
                            &Snapshot {
                                account_id,
                                listed: &mailboxes,
                                prior: &prior,
                                done: &done,
                            },
                            None,
                            Vec::new(),
                            Vec::new(),
                        )?;
                    }
                    continue;
                }
                (false, None)
            }
            session::FetchStop::Truncated { min_skipped_uid } => (false, min_skipped_uid),
        };
        let mut flags = Vec::new();
        let mut flags_complete = true;
        let needs_baseline = finished
            && same_validity
            && previous_owned
                .as_ref()
                .is_some_and(|prev| prev.modseq.is_none() && prev.uid > 0)
            && highest_modseq.is_some()
            && !matches!(plan, FetchPlan::Full | FetchPlan::ChangedSince(_));
        if needs_baseline {
            match session.uid_flags("1:*") {
                Ok(rows) => {
                    for (uid, flag_names) in rows {
                        let unread = !flag_names
                            .iter()
                            .any(|flag| flag.eq_ignore_ascii_case("\\Seen"));
                        flags.push(FlagUpdate {
                            remote_id: RemoteId(format!(
                                "imap:{uidvalidity}:{uid}:{}",
                                escape_mailbox(&mailbox.raw)
                            )),
                            unread,
                        });
                    }
                }
                Err(err) if uid_fetch_refused(&err) || untagged_overflow(&err) => {
                    flags_complete = false;
                }
                Err(err) => return Err(err),
            }
        }
        let mut uid = if finished {
            match &plan {
                FetchPlan::Full | FetchPlan::HighWater { .. } => match uidnext {
                    Some(next) => highest.unwrap_or(0).max(next.saturating_sub(1)),
                    None => checkpoint_uid(previous_owned.as_ref(), same_validity, highest),
                },
                FetchPlan::Discover { .. } | FetchPlan::ChangedSince(_) | FetchPlan::Idle => {
                    checkpoint_uid(previous_owned.as_ref(), same_validity, highest)
                }
            }
        } else {
            let partial = match min_skipped {
                Some(skipped) => Some(skipped.saturating_sub(1)),
                None => highest,
            };
            checkpoint_uid(previous_owned.as_ref(), same_validity, partial)
        };
        if let Some(missing) = min_unparsed {
            uid = uid.min(missing.saturating_sub(1));
        }
        // Record HIGHESTMODSEQ when this pass saw every message's flags: a full
        // or CHANGEDSINCE fetch, an empty mailbox, a high-water pass from uid 0,
        // or a UID FLAGS baseline for older mail that had no modseq yet.
        let saw_every_flag = finished
            && flags_complete
            && min_unparsed.is_none()
            && (matches!(plan, FetchPlan::Full | FetchPlan::ChangedSince(_))
                || exists == 0
                || needs_baseline
                || (matches!(plan, FetchPlan::HighWater { start } if start <= 1)
                    && previous_owned.as_ref().map(|prev| prev.uid).unwrap_or(0) == 0));
        let modseq = if saw_every_flag {
            highest_modseq.or_else(|| previous_owned.as_ref().and_then(|prev| prev.modseq))
        } else if same_validity {
            previous_owned.as_ref().and_then(|prev| prev.modseq)
        } else {
            None
        };
        let current = MailboxCheckpoint {
            uidvalidity,
            uid,
            modseq,
        };
        emit_progress(
            &mut persist,
            &Snapshot {
                account_id,
                listed: &mailboxes,
                prior: &prior,
                done: &done,
            },
            Some((&mailbox.raw, &current)),
            std::mem::take(&mut batch),
            flags,
        )?;
        done.insert(mailbox.raw.clone(), current);
    }
    session.logout();
    Ok(())
}

fn capped_highest(highest: Option<u32>, min_unparsed: Option<u32>) -> Option<u32> {
    match min_unparsed {
        Some(missing) => Some(
            highest
                .map(|uid| uid.min(missing.saturating_sub(1)))
                .unwrap_or(missing.saturating_sub(1)),
        ),
        None => highest,
    }
}

fn stream_bodies(
    session: &mut session::Session,
    plan: &FetchPlan,
    mut on_fetched: impl FnMut(session::Fetched) -> Result<(), Error>,
) -> Result<session::FetchStop, Error> {
    match plan {
        FetchPlan::Idle => Ok(session::FetchStop::Done),
        FetchPlan::ChangedSince(modseq) => session.uid_fetch_each("1:*", Some(*modseq), on_fetched),
        FetchPlan::HighWater { start } => {
            session.uid_fetch_each(&format!("{start}:*"), None, on_fetched)
        }
        FetchPlan::Full => session.uid_fetch_each("1:*", None, on_fetched),
        FetchPlan::Discover { start } => {
            let found = match session.uid_find(&format!("{start}:*")) {
                Ok(uids) => uids,
                Err(err) if untagged_overflow(&err) => {
                    return Ok(session::FetchStop::Truncated {
                        min_skipped_uid: None,
                    });
                }
                Err(err) if uid_fetch_refused(&err) => return Ok(session::FetchStop::Refused),
                Err(err) => return Err(err),
            };
            for uid in found.into_iter().filter(|uid| *uid >= *start) {
                match session.uid_fetch_each(&uid.to_string(), None, &mut on_fetched)? {
                    session::FetchStop::Done => {}
                    other => return Ok(other),
                }
            }
            Ok(session::FetchStop::Done)
        }
    }
}

struct Snapshot<'a> {
    account_id: &'a AccountId,
    listed: &'a [session::Listed],
    prior: &'a HashMap<String, MailboxCheckpoint>,
    done: &'a HashMap<String, MailboxCheckpoint>,
}

fn emit_progress(
    persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error>,
    snapshot: &Snapshot<'_>,
    current: Option<(&str, &MailboxCheckpoint)>,
    messages: Vec<Message>,
    flags: Vec<FlagUpdate>,
) -> Result<(), Error> {
    let mut pairs = Vec::new();
    for mailbox in snapshot.listed {
        let checkpoint = if let Some((name, cp)) = current
            && name == mailbox.raw
        {
            Some(cp.clone())
        } else {
            snapshot
                .done
                .get(&mailbox.raw)
                .cloned()
                .or_else(|| snapshot.prior.get(&mailbox.raw).cloned())
        };
        if let Some(checkpoint) = checkpoint {
            pairs.push((mailbox.raw.clone(), checkpoint));
        }
    }
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    persist(MailboxDelta {
        messages,
        flags,
        cursor: Some(encode_cursor(snapshot.account_id.as_str(), &pairs)),
    })
}

/// Tagged NO/BAD on UID FETCH. The socket is still a valid IMAP session.
/// A dropped connection is not this error and must abort the sync.
fn uid_fetch_refused(err: &Error) -> bool {
    matches!(err, Error::Backend(message) if message.contains("UID FETCH failed:"))
}

/// The untagged-reply cap. The command was drained, so later mailboxes can run.
fn untagged_overflow(err: &Error) -> bool {
    matches!(err, Error::Backend(message) if message.contains("exceeded") && message.contains("untagged"))
}

fn inbox_rank(name: &str) -> u8 {
    if name.eq_ignore_ascii_case("INBOX") {
        0
    } else {
        1
    }
}

#[derive(Clone, Copy)]
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
