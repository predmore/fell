use crate::account::AccountId;
use crate::threading::ThreadId;

/// RFC 5322 `Message-ID` (angle brackets optional; stored normalized).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageId(pub String);

impl MessageId {
    pub fn new(raw: impl AsRef<str>) -> Self {
        Self(normalize_message_id(raw.as_ref()).to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Backend-native id (IMAP UID, Proton message id, fixture id).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

/// Headers used for threading and display. Body may be filled in later by sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageHeaders {
    pub message_id: MessageId,
    pub proton_conversation_id: Option<String>,
    pub in_reply_to: Option<MessageId>,
    pub references: Vec<MessageId>,
    pub from: Address,
    pub subject: String,
    pub date_unix: i64,
}

impl MessageHeaders {
    /// Parent candidates, nearest first: `In-Reply-To`, then `References` reversed.
    pub fn parent_ids(&self) -> impl Iterator<Item = &MessageId> {
        self.in_reply_to.iter().chain(self.references.iter().rev())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub account_id: AccountId,
    pub headers: MessageHeaders,
    pub remote_id: RemoteId,
    pub thread_id: ThreadId,
    pub snippet: String,
    pub body_text: Option<String>,
    pub unread: bool,
    /// Folders this message was stored in (`Sent`, `INBOX`, …).
    pub folders: Vec<String>,
    /// Labels applied on any copy of this message.
    pub labels: Vec<String>,
    /// Addresses this copy arrived through (the account, an alias, a list).
    pub arrived_via: Vec<String>,
    /// `To` recipients. Searched by `to:` along with `cc` and `bcc`.
    pub to: Vec<Address>,
    /// `Cc` recipients.
    pub cc: Vec<Address>,
    /// `Bcc` recipients.
    pub bcc: Vec<Address>,
    /// User star. This is not a label; chips stay on `labels`.
    pub starred: bool,
    /// True when any stored copy of this message has an attachment.
    pub has_attachment: bool,
    /// Local archive bit. A later copy of the same Message-ID does not clear it.
    pub archived: bool,
    /// Unix seconds until which this message stays snoozed. `None` is not snoozed.
    pub snooze_until: Option<i64>,
    /// Set when a mailing list rewrote this copy (`List-Id`).
    pub list_id: Option<String>,
    /// `Reply-To` addresses. A reply goes to these instead of `from`.
    pub reply_to: Vec<Address>,
    /// File names of the message's attachments, for a forward to list.
    pub attachment_names: Vec<String>,
    /// The body parts sync parsed from the server copy. `None` when the
    /// source had no MIME to read (fixtures, local writes), which leaves any
    /// stored parts alone. Store reads leave it `None` so a folder list never
    /// carries bodies; [`crate::Store::message_body`] reads them.
    pub parts: Option<MessageParts>,
}

/// What a received message shows in the conversation view: its HTML and
/// plain-text parts, decoded and otherwise as sent, plus the few headers the
/// renderer uses to spot bulk mail. HTML is untrusted and is sanitized when
/// it is rendered, never here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageParts {
    /// The `text/html` body parts, in reading order. `None` when there is none.
    pub html: Option<String>,
    /// The `text/plain` body parts, in reading order. `None` for HTML-only
    /// mail; `body_text` then holds the HTML flattened to text.
    pub plain: Option<String>,
    /// Bulk-mail signals (`List-Unsubscribe`, `Precedence`, ESP markers) as
    /// `(name, value)`, unfolded, in header order.
    pub bulk_headers: Vec<(String, String)>,
    /// Every MIME part that is not body text, in document order: files,
    /// images the HTML shows by `cid:`, and signatures. Metadata only; the
    /// bytes stay on the server until [`crate::parts::fetch_part`] asks.
    pub mime_parts: Vec<MimePart>,
}

/// One MIME part of a received message other than its text and HTML body.
///
/// Everything here came from the message and is untrusted. `filename` in
/// particular must go through [`crate::parts::sanitize_filename`] before it
/// names a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimePart {
    /// The IMAP part number (RFC 3501 section 6.4.5), such as `2` or `3.1`:
    /// what `BODY.PEEK[<section>]` fetches from a server copy.
    pub section: String,
    pub role: PartRole,
    /// `type/subtype`, lowercase. `application/octet-stream` when the part
    /// gave none that reads.
    pub mime_type: String,
    /// The name the sender gave it (`filename`, else `name`), with RFC 2231
    /// and encoded words decoded.
    pub filename: Option<String>,
    /// Bytes once the transfer encoding is undone.
    pub size: u64,
    /// Bytes on the server, still transfer-encoded: what a fetch downloads.
    pub encoded_size: u64,
    pub encoding: TransferEncoding,
    /// `Content-ID` without angle brackets: what a `cid:` URL names.
    pub content_id: Option<String>,
    pub disposition: Disposition,
    /// The part sits inside an attached `message/rfc822` (the attached
    /// message itself is not nested).
    pub nested: bool,
}

/// What a [`MimePart`] is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartRole {
    /// A file offered to the reader. Sets `has_attachment`.
    Attachment,
    /// An image shown inside the HTML body, which loads its `Content-ID` by
    /// a `cid:` URL (`src`, `background`, `poster`, CSS `url()`), and not
    /// sent as `Content-Disposition: attachment`. Not an attachment.
    Inline,
    /// A detached signature: `application/pgp-signature`, an S/MIME
    /// signature type, or a part named `*.p7s` or `signature.asc`. Not an
    /// attachment.
    Signature,
}

/// The `Content-Transfer-Encoding` a part's bytes are stored in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TransferEncoding {
    /// `7bit`, `8bit`, `binary`, none given, or one not known.
    #[default]
    Identity,
    QuotedPrintable,
    Base64,
}

/// The `Content-Disposition` type, when the part gave one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Disposition {
    #[default]
    Unspecified,
    Inline,
    Attachment,
}

pub(crate) fn encode_addresses(addrs: &[Address]) -> String {
    addrs
        .iter()
        .filter(|addr| !addr.email.trim().is_empty())
        .map(|addr| {
            let name = addr
                .name
                .as_deref()
                .unwrap_or("")
                .replace(['\u{1e}', '\u{1f}'], " ");
            let email = addr.email.trim().replace(['\u{1e}', '\u{1f}'], " ");
            format!("{name}\u{1f}{email}")
        })
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

pub(crate) fn decode_addresses(raw: &str) -> Vec<Address> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split('\u{1e}')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (name, email) = part.split_once('\u{1f}').unwrap_or(("", part));
            Address {
                name: if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                },
                email: email.to_string(),
            }
        })
        .collect()
}

pub(crate) fn normalize_message_id(raw: &str) -> &str {
    raw.trim().trim_start_matches('<').trim_end_matches('>')
}

/// Exact identity of a body. Two copies match only when this string is equal.
///
/// Whitespace runs collapse so a list that reflows the same text still matches.
/// Empty text has no fingerprint and never collapses. Subject is not an input.
pub(crate) fn body_fingerprint(body: &str) -> String {
    let normalized = normalize_body(body);
    if normalized.is_empty() {
        return String::new();
    }
    let bytes = normalized.as_bytes();
    let hi = fnv1a64(bytes, 0xcbf29ce484222325);
    let lo = fnv1a64(bytes, 0x8422_2325_cbf2_9ce4);
    format!("{hi:016x}{lo:016x}")
}

/// `+1` and `Thanks!` are not unique. A longer body can still identify one send.
const SHORT_REPLY_MAX_CHARS: usize = 8;

/// True when `body` is too short to be the only identity of a message.
pub(crate) fn is_short_reply_body(body: &str) -> bool {
    let len = normalize_body(body).chars().count();
    (1..=SHORT_REPLY_MAX_CHARS).contains(&len)
}

fn normalize_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut pending_space = false;
    for ch in body.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

fn fnv1a64(bytes: &[u8], offset: u64) -> u64 {
    let mut hash = offset;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_angle_brackets() {
        assert_eq!(MessageId::new("<a@b>").as_str(), "a@b");
        assert_eq!(MessageId::new(" a@b ").as_str(), "a@b");
    }

    #[test]
    fn fingerprint_is_exact_after_whitespace_and_empty_has_none() {
        assert_eq!(
            body_fingerprint("hello   body\n"),
            body_fingerprint("hello body")
        );
        assert!(body_fingerprint("  \n").is_empty());
        assert_ne!(body_fingerprint("hello"), body_fingerprint("hello!"));
        assert_eq!(body_fingerprint("会議"), body_fingerprint("会議"));
    }

    #[test]
    fn short_reply_bodies_are_a_few_characters() {
        assert!(is_short_reply_body("+1"));
        assert!(is_short_reply_body("  Thanks! \n"));
        assert!(is_short_reply_body("ありがとう"));
        assert!(!is_short_reply_body("hello body"));
        assert!(!is_short_reply_body("   "));
    }
}
