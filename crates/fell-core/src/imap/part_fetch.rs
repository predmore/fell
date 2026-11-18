//! Fetch one MIME part of a stored copy by its IMAP section, on demand.
//!
//! A short-lived read-only session: `EXAMINE`, then `UID FETCH` with
//! `BODY.PEEK[<section>]<offset.len>` in windows, so nothing is marked read,
//! the caller sees progress after each window. A server that ignores the
//! window sends the whole part in one reply, which is read like a body
//! (up to the 32 MB literal cap). The bytes come back still
//! transfer-encoded; decoding is the caller's job.

use crate::Error;
use crate::message::RemoteId;

use super::{ImapConfig, parse_remote_id, session, unescape_mailbox};

/// Bytes asked for per `UID FETCH`.
const WINDOW: usize = 2 * 1024 * 1024;

/// A part never grows past this on the wire; anything longer is refused.
pub(crate) const MAX_ENCODED_PART: usize = 96 * 1024 * 1024;

fn window() -> usize {
    #[cfg(test)]
    {
        TEST_WINDOW.with(|window| window.get())
    }
    #[cfg(not(test))]
    {
        WINDOW
    }
}

#[cfg(test)]
thread_local! {
    static TEST_WINDOW: std::cell::Cell<usize> = const { std::cell::Cell::new(WINDOW) };
}

#[cfg(test)]
pub(super) fn set_window(bytes: usize) {
    TEST_WINDOW.with(|window| window.set(bytes));
}

/// Whether `section` is an IMAP part number (`1`, `2.3`), the only form
/// that goes into a command.
pub(crate) fn valid_section(section: &str) -> bool {
    !section.is_empty()
        && section.len() <= 64
        && section.split('.').all(|number| {
            !number.is_empty()
                && number.len() <= 5
                && number.bytes().all(|b| b.is_ascii_digit())
                && !number.starts_with('0')
        })
}

/// The encoded bytes of part `section` of `copy`, listed as `encoded_size`
/// bytes on the wire (0 when unknown). `Ok(None)` when the server no longer
/// has that copy (expunged, a new UIDVALIDITY, a refused mailbox).
/// `progress` hears the bytes read so far after each window.
pub(crate) fn fetch_part(
    config: &ImapConfig,
    copy: &RemoteId,
    section: &str,
    encoded_size: u64,
    progress: &mut dyn FnMut(u64),
) -> Result<Option<Vec<u8>>, Error> {
    if !valid_section(section) {
        return Err(Error::Backend(format!(
            "imap: {section:?} is not a part number"
        )));
    }
    let Some((mailbox, uidvalidity, uid)) = parse_remote_id(&copy.0)
        .and_then(|copy| Some((unescape_mailbox(&copy.mailbox)?, copy.uidvalidity, copy.uid)))
    else {
        return Ok(None);
    };
    let mut session = session::Session::open(config)?;
    let expected = usize::try_from(encoded_size).unwrap_or(usize::MAX);
    let result = fetch_windows(
        &mut session,
        &mailbox,
        uidvalidity,
        uid,
        section,
        expected,
        progress,
    );
    session.logout();
    result
}

fn fetch_windows(
    session: &mut session::Session,
    mailbox: &str,
    uidvalidity: u32,
    uid: u32,
    section: &str,
    expected: usize,
    progress: &mut dyn FnMut(u64),
) -> Result<Option<Vec<u8>>, Error> {
    match session.examine(mailbox)? {
        Some(examined) if examined.uidvalidity == uidvalidity => {}
        _ => return Ok(None),
    }
    let too_large = || {
        Error::Backend(format!(
            "imap: part {section} is larger than {} MB",
            MAX_ENCODED_PART / (1024 * 1024)
        ))
    };
    if expected > MAX_ENCODED_PART {
        return Err(too_large());
    }
    let window = window();
    let mut out: Vec<u8> = Vec::new();
    loop {
        let offset = out.len();
        let Some(reply) = session.uid_fetch_part(uid, section, offset, window)? else {
            if out.is_empty() {
                return Ok(None);
            }
            return Err(Error::Backend(format!(
                "imap: UID {uid} in {mailbox} went away while part {section} was read"
            )));
        };
        let done = match reply.origin {
            Some(origin) if origin == offset => {
                let short = reply.bytes.len() < window;
                out.extend_from_slice(&reply.bytes);
                short
            }
            // No origin: the server ignored the window and sent the whole
            // part, from its first byte.
            None => {
                out = reply.bytes;
                true
            }
            Some(origin) => {
                return Err(Error::Backend(format!(
                    "imap: asked for part {section} from byte {offset}, got it from {origin}"
                )));
            }
        };
        if out.len() > MAX_ENCODED_PART {
            return Err(too_large());
        }
        progress(out.len() as u64);
        // A part exactly one window long is whole once its listed size is
        // in: no second request for an empty window.
        if done || (expected > 0 && out.len() >= expected) {
            return Ok(Some(out));
        }
    }
}
