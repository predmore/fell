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

/// How far back a reply with no threading headers looks for its original.
pub(crate) const SUBJECT_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;

/// Reply and forward markers across common mail clients and languages.
/// Longer markers come first so `fwd` wins over `fw`.
const REPLY_MARKERS: &[&str] = &[
    "antw", "odp", "fwd", "rif", "ref", "re", "fw", "aw", "wg", "sv", "vs", "tr", "r",
];
const CJK_MARKERS: &[&str] = &["回复", "回覆", "答复", "答覆", "转发", "轉寄"];

/// The subject a reply shares with its original, and whether `subject`
/// reads as a reply or forward.
///
/// Strips any run of markers such as `Re:`, `RE[2]:`, `AW:`, `Fwd:`,
/// `回复：`, and list tags like `[team]`, then folds case and spacing.
pub(crate) fn thread_subject(subject: &str) -> (String, bool) {
    let mut rest = subject.trim();
    let mut reply = false;
    loop {
        if let Some(after) = strip_marker(rest) {
            rest = after.trim_start();
            reply = true;
            continue;
        }
        if let Some(after) = strip_list_tag(rest) {
            rest = after.trim_start();
            continue;
        }
        break;
    }
    let key = rest
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    (key, reply)
}

fn strip_marker(subject: &str) -> Option<&str> {
    for marker in CJK_MARKERS {
        if let Some(after) = subject.strip_prefix(marker) {
            let after = after.trim_start();
            if let Some(rest) = after.strip_prefix(':').or_else(|| after.strip_prefix('：')) {
                return Some(rest);
            }
        }
    }
    // Markers are short ASCII. Look at the first few bytes, cut on a
    // character boundary so a multibyte letter right after `Re: ` is fine.
    let mut end = subject.len().min(8);
    while !subject.is_char_boundary(end) {
        end -= 1;
    }
    let lower = subject[..end].to_ascii_lowercase();
    for marker in REPLY_MARKERS {
        if !lower.starts_with(marker) {
            continue;
        }
        let mut after = &subject[marker.len()..];
        // `RE[2]:` and `Re(3):` count replies.
        for (open, close) in [('[', ']'), ('(', ')')] {
            if let Some(inner) = after.strip_prefix(open)
                && let Some(end) = inner.find(close)
                && end > 0
                && inner[..end].chars().all(|ch| ch.is_ascii_digit())
            {
                after = &inner[end + 1..];
            }
        }
        let after = after.trim_start_matches(' ');
        if let Some(rest) = after.strip_prefix(':').or_else(|| after.strip_prefix('：')) {
            return Some(rest);
        }
    }
    None
}

/// A leading `[team]` list tag. A tag with a digit is kept: it is usually a
/// ticket or patch id (`[PROJ-12]`, `[PATCH v2]`), and stripping it would join
/// different tickets that share a title.
fn strip_list_tag(subject: &str) -> Option<&str> {
    let inner = subject.strip_prefix('[')?;
    let end = inner.find(']')?;
    let tag = &inner[..end];
    (end > 0 && end <= 80 && !tag.chars().any(|ch| ch.is_ascii_digit())).then(|| &inner[end + 1..])
}

/// Subjects too common to join two messages on their own.
pub(crate) fn subject_is_generic(key: &str) -> bool {
    const GENERIC: &[&str] = &[
        "",
        "hi",
        "hello",
        "hey",
        "thanks",
        "thank you",
        "question",
        "quick question",
        "fyi",
        "test",
        "update",
        "meeting",
        "call",
        "no subject",
        "(no subject)",
        "invoice",
        "receipt",
        "your order",
    ];
    key.chars().count() < 4 || GENERIC.contains(&key)
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

    #[test]
    fn reply_markers_strip_across_clients() {
        for (subject, key, reply) in [
            ("Q3 numbers", "q3 numbers", false),
            ("Re: Q3 numbers", "q3 numbers", true),
            ("RE: RE: Fwd: Q3  numbers", "q3 numbers", true),
            ("AW: Q3 numbers", "q3 numbers", true),
            ("SV: Q3 numbers", "q3 numbers", true),
            ("RE[2]: Q3 numbers", "q3 numbers", true),
            ("Re(3): Q3 numbers", "q3 numbers", true),
            ("[team] Re: Q3 numbers", "q3 numbers", true),
            ("Re: [team] Q3 numbers", "q3 numbers", true),
            ("回复：Q3 numbers", "q3 numbers", true),
            ("答复: Q3 numbers", "q3 numbers", true),
            ("Reunion plans", "reunion plans", false),
            ("Fwd Q3 numbers", "fwd q3 numbers", false),
            ("Refund: order 12", "refund: order 12", false),
        ] {
            assert_eq!(
                thread_subject(subject),
                (key.to_string(), reply),
                "{subject}"
            );
        }
    }

    #[test]
    fn short_and_stock_subjects_are_generic() {
        assert!(subject_is_generic(""));
        assert!(subject_is_generic("hi"));
        assert!(subject_is_generic("thanks"));
        assert!(subject_is_generic(&thread_subject("Re: Quick question").0));
        assert!(!subject_is_generic("q3 numbers"));
    }

    #[test]
    fn reply_marker_survives_a_multibyte_character_at_byte_eight() {
        assert_eq!(
            thread_subject("Re: 中文邮件"),
            ("中文邮件".to_string(), true)
        );
        assert_eq!(
            thread_subject("RE[2]: 中文邮件"),
            ("中文邮件".to_string(), true)
        );
    }

    #[test]
    fn ticket_tags_stay_part_of_the_subject() {
        assert_eq!(
            thread_subject("[PROJ-1] Login fails").0,
            "[proj-1] login fails"
        );
        assert_eq!(
            thread_subject("Re: [PROJ-2] Login fails").0,
            "[proj-2] login fails"
        );
        assert_eq!(thread_subject("[team] Login fails").0, "login fails");
    }
}
