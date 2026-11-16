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
    /// Set when a mailing list rewrote this copy's headers.
    pub list_id: Option<String>,
}

pub(crate) fn normalize_message_id(raw: &str) -> &str {
    raw.trim().trim_start_matches('<').trim_end_matches('>')
}

/// Stable identity of a body. Whitespace runs collapse; empty text has no fingerprint.
///
/// Used when a mailing list rewrites `Message-ID` but leaves the body intact.
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
    fn body_fingerprint_collapses_whitespace_only() {
        assert_eq!(
            body_fingerprint("Hello   world\n"),
            body_fingerprint("Hello world")
        );
        assert!(body_fingerprint(" \n\t").is_empty());
        assert_ne!(body_fingerprint("Hello"), body_fingerprint("hello"));
        assert_ne!(body_fingerprint("Hello"), body_fingerprint("Hello!"));
    }
}
