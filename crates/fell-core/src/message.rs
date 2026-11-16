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
}

pub(crate) fn normalize_message_id(raw: &str) -> &str {
    raw.trim().trim_start_matches('<').trim_end_matches('>')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_angle_brackets() {
        assert_eq!(MessageId::new("<a@b>").as_str(), "a@b");
        assert_eq!(MessageId::new(" a@b ").as_str(), "a@b");
    }
}
