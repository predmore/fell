use crate::message::{MessageHeaders, MessageId};

/// Local thread key. Proton conversation ids are preferred over header walks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadId(pub String);

impl ThreadId {
    pub fn proton(conversation_id: &str) -> Self {
        Self(format!("proton:{conversation_id}"))
    }

    pub fn from_message_id(message_id: &MessageId) -> Self {
        Self(format!("msgid:{}", message_id.as_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Assign a thread id for `headers`.
///
/// 1. Proton conversation id, when present.
/// 2. Else the thread of the first stored parent (`In-Reply-To` / `References`).
/// 3. Else a new thread keyed by this message's `Message-ID`.
pub fn resolve_thread_id(
    headers: &MessageHeaders,
    lookup_parent: impl Fn(&MessageId) -> Option<ThreadId>,
) -> ThreadId {
    if let Some(id) = headers
        .proton_conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return ThreadId::proton(id);
    }

    for parent in headers.parent_ids() {
        if let Some(thread) = lookup_parent(parent) {
            return thread;
        }
    }

    ThreadId::from_message_id(&headers.message_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Address;
    use std::collections::HashMap;

    fn headers(
        id: &str,
        proton: Option<&str>,
        in_reply_to: Option<&str>,
        references: &[&str],
    ) -> MessageHeaders {
        MessageHeaders {
            message_id: MessageId::new(id),
            proton_conversation_id: proton.map(str::to_string),
            in_reply_to: in_reply_to.map(MessageId::new),
            references: references.iter().copied().map(MessageId::new).collect(),
            from: Address {
                name: None,
                email: "a@b".into(),
            },
            subject: "s".into(),
            date_unix: 0,
        }
    }

    #[test]
    fn proton_id_wins_over_headers() {
        let h = headers("child@x", Some("conv-1"), Some("parent@x"), &[]);
        let thread = resolve_thread_id(&h, |_| panic!("must not walk headers"));
        assert_eq!(thread, ThreadId::proton("conv-1"));
    }

    #[test]
    fn reply_joins_parent_thread() {
        let parent_thread = ThreadId::from_message_id(&MessageId::new("root@x"));
        let map = HashMap::from([(MessageId::new("root@x"), parent_thread.clone())]);
        let h = headers("child@x", None, Some("root@x"), &["root@x"]);
        let thread = resolve_thread_id(&h, |id| map.get(id).cloned());
        assert_eq!(thread, parent_thread);
    }

    #[test]
    fn references_join_when_in_reply_to_missing() {
        let parent_thread = ThreadId::from_message_id(&MessageId::new("root@x"));
        let map = HashMap::from([(MessageId::new("root@x"), parent_thread.clone())]);
        let h = headers("child@x", None, None, &["root@x", "mid@x"]);
        let thread = resolve_thread_id(&h, |id| map.get(id).cloned());
        assert_eq!(thread, parent_thread);
    }

    #[test]
    fn new_thread_when_no_parents() {
        let h = headers("solo@x", None, None, &[]);
        let thread = resolve_thread_id(&h, |_| None);
        assert_eq!(thread, ThreadId::from_message_id(&MessageId::new("solo@x")));
    }
}
