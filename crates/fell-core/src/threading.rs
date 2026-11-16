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
///
/// Parent lookup errors propagate; a missing parent is `Ok(None)`, not an error.
pub fn resolve_thread_id<E>(
    headers: &MessageHeaders,
    mut lookup_parent: impl FnMut(&MessageId) -> Result<Option<ThreadId>, E>,
) -> Result<ThreadId, E> {
    if let Some(id) = headers
        .proton_conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(ThreadId::proton(id));
    }

    for parent in headers.parent_ids() {
        if let Some(thread) = lookup_parent(parent)? {
            return Ok(thread);
        }
    }

    Ok(ThreadId::from_message_id(&headers.message_id))
}

/// Thread of the nearest stored header parent (`In-Reply-To` / `References`).
/// Ignores Proton conversation ids so a proton-tagged reply can still join
/// the parent's header thread.
pub fn header_parent_thread_id<E>(
    headers: &MessageHeaders,
    mut lookup_parent: impl FnMut(&MessageId) -> Result<Option<ThreadId>, E>,
) -> Result<Option<ThreadId>, E> {
    for parent in headers.parent_ids() {
        if let Some(thread) = lookup_parent(parent)? {
            return Ok(Some(thread));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Address;
    use std::collections::HashMap;
    use std::convert::Infallible;

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

    fn lookup<'a>(
        map: &'a HashMap<MessageId, ThreadId>,
    ) -> impl FnMut(&MessageId) -> Result<Option<ThreadId>, Infallible> + 'a {
        |id| Ok(map.get(id).cloned())
    }

    #[test]
    fn proton_id_wins_over_headers() {
        let h = headers("child@x", Some("conv-1"), Some("parent@x"), &[]);
        let thread = resolve_thread_id(&h, |_| -> Result<_, Infallible> {
            panic!("must not walk headers")
        })
        .unwrap();
        assert_eq!(thread, ThreadId::proton("conv-1"));
    }

    #[test]
    fn reply_joins_parent_thread() {
        let parent_thread = ThreadId::from_message_id(&MessageId::new("root@x"));
        let map = HashMap::from([(MessageId::new("root@x"), parent_thread.clone())]);
        let h = headers("child@x", None, Some("root@x"), &["root@x"]);
        let thread = resolve_thread_id(&h, lookup(&map)).unwrap();
        assert_eq!(thread, parent_thread);
    }

    #[test]
    fn references_join_when_in_reply_to_missing() {
        let parent_thread = ThreadId::from_message_id(&MessageId::new("root@x"));
        let map = HashMap::from([(MessageId::new("root@x"), parent_thread.clone())]);
        let h = headers("child@x", None, None, &["root@x", "mid@x"]);
        let thread = resolve_thread_id(&h, lookup(&map)).unwrap();
        assert_eq!(thread, parent_thread);
    }

    #[test]
    fn new_thread_when_no_parents() {
        let h = headers("solo@x", None, None, &[]);
        let thread = resolve_thread_id(&h, |_| -> Result<_, Infallible> { Ok(None) }).unwrap();
        assert_eq!(thread, ThreadId::from_message_id(&MessageId::new("solo@x")));
    }

    #[test]
    fn header_parent_ignores_proton_id() {
        let parent_thread = ThreadId::from_message_id(&MessageId::new("root@x"));
        let map = HashMap::from([(MessageId::new("root@x"), parent_thread.clone())]);
        let h = headers("child@x", Some("conv"), Some("root@x"), &[]);
        assert_eq!(
            header_parent_thread_id(&h, lookup(&map)).unwrap(),
            Some(parent_thread)
        );
        assert_eq!(
            resolve_thread_id(&h, lookup(&map)).unwrap(),
            ThreadId::proton("conv")
        );
    }
}
