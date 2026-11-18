//! Reply, reply all and forward: the fields, References, and quoted
//! original a new draft starts from.
//!
//! Nothing here reads or writes the mailbox. The caller looks the parent up
//! and starts the draft.

use std::collections::HashSet;

use crate::message::{Address, Message, MessageId};

use super::frontmatter::DraftFields;
use super::markdown::escape;
use super::mime::rfc5322_date;

/// Which kind of answer a draft starts as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReplyKind {
    Reply,
    ReplyAll,
    Forward,
}

/// What a reply or forward keeps beside its fields. Fell sets these; the
/// draft file does not carry them, so an edit cannot change them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplyParts {
    /// The sent message's References, oldest first, ending with the parent.
    /// Empty for a new message or a forward.
    pub references: Vec<String>,
    /// The quoted original as plain text. Sent after the body in the
    /// text/plain part.
    pub quote_text: String,
    /// The same quote as HTML. Sent after the body in the HTML part, and
    /// painted collapsed below the draft.
    pub quote_html: String,
}

impl ReplyParts {
    pub fn is_empty(&self) -> bool {
        self.references.is_empty() && self.quote_text.is_empty() && self.quote_html.is_empty()
    }
}

/// A reply or forward, ready to start as a draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyStart {
    pub fields: DraftFields,
    pub parts: ReplyParts,
}

/// The most ids a References header keeps. The first (the thread's root)
/// and the newest are kept; ids in between go first.
pub const MAX_REFERENCES: usize = 20;

/// An id longer than this is not a Message-ID worth sending on.
const MAX_ID_LEN: usize = 250;

/// Start a `kind` answer to `parent` from `from`. `own` holds the person's
/// own addresses, which a reply never sends to.
pub fn start_reply(kind: ReplyKind, parent: &Message, from: &str, own: &[String]) -> ReplyStart {
    let subject = &parent.headers.subject;
    let parent_id = parent.headers.message_id.as_str();
    match kind {
        ReplyKind::Reply | ReplyKind::ReplyAll => {
            let (to, cc) = reply_recipients(parent, own, kind == ReplyKind::ReplyAll);
            let references: Vec<String> = parent
                .headers
                .references
                .iter()
                .map(|id| id.as_str().to_string())
                .collect();
            let (quote_text, quote_html) = reply_quote(parent);
            ReplyStart {
                fields: DraftFields {
                    from: from.to_string(),
                    to,
                    cc,
                    subject: reply_subject(subject),
                    in_reply_to: Some(parent_id.to_string()).filter(|id| !id.is_empty()),
                    ..DraftFields::default()
                },
                parts: ReplyParts {
                    references: build_references(&references, parent_id),
                    quote_text,
                    quote_html,
                },
            }
        }
        ReplyKind::Forward => {
            let (quote_text, quote_html) = forward_quote(parent);
            ReplyStart {
                fields: DraftFields {
                    from: from.to_string(),
                    subject: forward_subject(subject),
                    ..DraftFields::default()
                },
                parts: ReplyParts {
                    references: Vec::new(),
                    quote_text,
                    quote_html,
                },
            }
        }
    }
}

/// `Re: ` and the subject without its reply and forward markers, so a reply
/// to `Re: Plans` is `Re: Plans`, not `Re: Re: Plans`.
pub fn reply_subject(subject: &str) -> String {
    prefixed("Re:", subject)
}

/// `Fwd: ` and the subject without its reply and forward markers.
pub fn forward_subject(subject: &str) -> String {
    prefixed("Fwd:", subject)
}

fn prefixed(marker: &str, subject: &str) -> String {
    let rest = crate::threading::strip_reply_markers(&one_line(subject));
    if rest.is_empty() {
        marker.to_string()
    } else {
        format!("{marker} {rest}")
    }
}

/// To and Cc for a reply to `parent`. To is Reply-To when the message has
/// one, else the sender. Reply all adds the other To recipients to To and
/// keeps Cc as Cc. The person's own addresses (`own`) and repeats are left
/// out. A reply to the person's own message goes to whoever it went to.
pub fn reply_recipients(parent: &Message, own: &[String], all: bool) -> (Vec<String>, Vec<String>) {
    let own: HashSet<String> = own
        .iter()
        .map(|email| email.trim().to_lowercase())
        .collect();
    let mine = |address: &Address| own.contains(&address.email.trim().to_lowercase());
    let from_me = mine(&parent.headers.from);
    let primary: Vec<&Address> = if from_me {
        parent.to.iter().collect()
    } else if !parent.reply_to.is_empty() {
        parent.reply_to.iter().collect()
    } else {
        vec![&parent.headers.from]
    };
    let mut seen = HashSet::new();
    let mut take = |addresses: Vec<&Address>| -> Vec<String> {
        addresses
            .into_iter()
            .filter(|address| !address.email.trim().is_empty() && !mine(address))
            .filter(|address| seen.insert(address.email.trim().to_lowercase()))
            .map(format_address)
            .collect()
    };
    let mut to = take(primary);
    if all && !from_me {
        to.extend(take(parent.to.iter().collect()));
    }
    let cc = if all {
        take(parent.cc.iter().collect())
    } else {
        Vec::new()
    };
    if to.is_empty() && cc.is_empty() {
        // Mail the person sent only to themselves: answer the sender.
        let from = &parent.headers.from;
        if !from.email.trim().is_empty() {
            to.push(format_address(from));
        }
    }
    (to, cc)
}

/// References for a reply: the parent's References and then the parent's
/// own Message-ID (RFC 5322 §3.6.4). Repeats and ids that cannot be sent are
/// dropped; a long chain keeps its root and its newest ids, at most
/// [`MAX_REFERENCES`] in all. Ids are bare, without angle brackets.
pub fn build_references(parent_references: &[String], parent_id: &str) -> Vec<String> {
    let clean = |raw: &str| {
        let id = MessageId::new(raw.trim()).as_str().to_string();
        (!id.is_empty()
            && id.len() <= MAX_ID_LEN
            && !id.chars().any(|ch| ch.is_whitespace() || ch.is_control())
            && !id.contains(['<', '>']))
        .then_some(id)
    };
    let parent = clean(parent_id);
    let mut seen = HashSet::new();
    let mut out: Vec<String> = parent_references
        .iter()
        .filter_map(|raw| clean(raw))
        .filter(|id| Some(id) != parent.as_ref())
        .filter(|id| seen.insert(id.clone()))
        .collect();
    out.extend(parent);
    if out.len() > MAX_REFERENCES {
        let newest = out.split_off(out.len() - (MAX_REFERENCES - 1));
        out.truncate(1);
        out.extend(newest);
    }
    out
}

/// An address as a draft's To or Cc line holds it: `Name <email>`, with the
/// name in quotes when it has a comma or another special character.
fn format_address(address: &Address) -> String {
    let email = address.email.trim().to_string();
    let name: String = address
        .name
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '"' && *ch != '\\')
        .collect();
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || name.eq_ignore_ascii_case(&email) {
        return email;
    }
    if name.contains([',', ';', ':', '<', '>', '@', '(', ')', '[', ']']) {
        format!("\"{name}\" <{email}>")
    } else {
        format!("{name} <{email}>")
    }
}

fn display_address(address: &Address) -> String {
    match address.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() && !name.eq_ignore_ascii_case(&address.email) => {
            format!("{} <{}>", one_line(name), address.email.trim())
        }
        _ => address.email.trim().to_string(),
    }
}

fn display_addresses(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(display_address)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The original's text, as the conversation shows it.
fn original_text(parent: &Message) -> String {
    let body = parent
        .body_text
        .as_deref()
        .filter(|body| !body.trim().is_empty())
        .unwrap_or(&parent.snippet);
    body.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end()
        .to_string()
}

/// "On <date>, <sender> wrote:" and the original with `> ` before each line.
fn reply_quote(parent: &Message) -> (String, String) {
    let intro = format!(
        "On {}, {} wrote:",
        rfc5322_date(parent.headers.date_unix),
        display_address(&parent.headers.from)
    );
    let body = original_text(parent);
    let mut text = format!("{intro}\n");
    for line in body.lines() {
        if line.is_empty() {
            text.push_str(">\n");
        } else if line.starts_with('>') {
            text.push_str(&format!(">{line}\n"));
        } else {
            text.push_str(&format!("> {line}\n"));
        }
    }
    let html = format!(
        "<p>{}</p><blockquote>{}</blockquote>",
        escape(&intro),
        paragraphs(&body)
    );
    (text, html)
}

/// The forwarded header block and the original, unquoted. Attachments are
/// named in the block: Fell does not keep their bytes, so they do not go
/// with the forward.
fn forward_quote(parent: &Message) -> (String, String) {
    let mut header = vec![
        "---------- Forwarded message ----------".to_string(),
        format!("From: {}", display_address(&parent.headers.from)),
        format!("Date: {}", rfc5322_date(parent.headers.date_unix)),
        format!("Subject: {}", one_line(&parent.headers.subject)),
    ];
    if !parent.to.is_empty() {
        header.push(format!("To: {}", display_addresses(&parent.to)));
    }
    if !parent.cc.is_empty() {
        header.push(format!("Cc: {}", display_addresses(&parent.cc)));
    }
    if let Some(line) = attachment_line(parent) {
        header.push(line);
    }
    let body = original_text(parent);
    let text = format!("{}\n\n{body}\n", header.join("\n"));
    let html = format!(
        "<p>{}</p><blockquote>{}</blockquote>",
        header
            .iter()
            .map(|line| escape(line))
            .collect::<Vec<_>>()
            .join("<br>"),
        paragraphs(&body)
    );
    (text, html)
}

/// "Attachments (not forwarded): a.pdf, b.png", when the original had any.
pub fn attachment_line(parent: &Message) -> Option<String> {
    let names: Vec<String> = parent
        .attachment_names
        .iter()
        .map(|name| one_line(name))
        .filter(|name| !name.is_empty())
        .collect();
    if !names.is_empty() {
        Some(format!("Attachments (not forwarded): {}", names.join(", ")))
    } else if parent.has_attachment {
        Some("Attachments (not forwarded): the original had attachments".to_string())
    } else {
        None
    }
}

/// Plain text as HTML paragraphs: a blank line starts a new one, and lines
/// inside one are kept with `<br>`.
fn paragraphs(text: &str) -> String {
    let mut out = String::new();
    for block in text.split("\n\n") {
        let lines: Vec<String> = block.lines().map(|line| escape(line.trim_end())).collect();
        if lines.iter().all(|line| line.trim().is_empty()) {
            continue;
        }
        out.push_str("<p>");
        out.push_str(&lines.join("<br>"));
        out.push_str("</p>");
    }
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountId, MessageHeaders, RemoteId, ThreadId};

    fn address(name: Option<&str>, email: &str) -> Address {
        Address {
            name: name.map(str::to_string),
            email: email.to_string(),
        }
    }

    fn parent() -> Message {
        let id = MessageId::new("parent@x");
        Message {
            account_id: AccountId("a".into()),
            headers: MessageHeaders {
                message_id: id.clone(),
                proton_conversation_id: None,
                in_reply_to: Some(MessageId::new("middle@x")),
                references: vec![MessageId::new("root@x"), MessageId::new("middle@x")],
                from: address(Some("Ada Lovelace"), "ada@x"),
                subject: "Re: Plans".into(),
                date_unix: 1_700_000_000,
            },
            remote_id: RemoteId("r".into()),
            thread_id: ThreadId::from_message_id(&id),
            snippet: "See you".into(),
            body_text: Some("See you at noon.\n\n> earlier\n".into()),
            unread: false,
            folders: vec!["INBOX".into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: vec![
                address(Some("Jon"), "jon@example.com"),
                address(Some("Maya"), "maya@x"),
            ],
            cc: vec![
                address(None, "sam@x"),
                address(Some("Ada again"), "ADA@x"),
                address(Some("Jon"), "Jon@Example.com"),
                address(None, "sam@x"),
            ],
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
        }
    }

    fn me() -> Vec<String> {
        vec!["jon@example.com".into()]
    }

    #[test]
    fn a_reply_goes_to_the_sender_and_never_to_me() {
        let (to, cc) = reply_recipients(&parent(), &me(), false);
        assert_eq!(to, vec!["Ada Lovelace <ada@x>"]);
        assert!(cc.is_empty());
    }

    #[test]
    fn reply_to_wins_over_the_sender() {
        let mut message = parent();
        message.reply_to = vec![address(Some("Plans list"), "plans@lists.x")];
        let (to, _) = reply_recipients(&message, &me(), false);
        assert_eq!(to, vec!["Plans list <plans@lists.x>"]);
        let (to, cc) = reply_recipients(&message, &me(), true);
        assert_eq!(
            to,
            vec!["Plans list <plans@lists.x>", "Maya <maya@x>"],
            "the sender is not added back beside Reply-To"
        );
        assert_eq!(cc, vec!["sam@x", "Ada again <ADA@x>"]);
    }

    #[test]
    fn reply_all_adds_the_others_once_each_without_me() {
        let (to, cc) = reply_recipients(&parent(), &me(), true);
        assert_eq!(to, vec!["Ada Lovelace <ada@x>", "Maya <maya@x>"]);
        // Ada is in To already and Jon is me, in any case; Sam is listed twice.
        assert_eq!(cc, vec!["sam@x"]);
    }

    #[test]
    fn a_reply_to_my_own_message_goes_to_its_recipients() {
        let mut message = parent();
        message.headers.from = address(Some("Jon"), "JON@example.com");
        let (to, cc) = reply_recipients(&message, &me(), false);
        assert_eq!(to, vec!["Maya <maya@x>"]);
        assert!(cc.is_empty());
        let (to, cc) = reply_recipients(&message, &me(), true);
        assert_eq!(to, vec!["Maya <maya@x>"]);
        assert_eq!(cc, vec!["sam@x", "Ada again <ADA@x>"]);
        // A note to myself alone is answered to me.
        message.to = vec![address(None, "jon@example.com")];
        message.cc.clear();
        let (to, _) = reply_recipients(&message, &me(), true);
        assert_eq!(to, vec!["Jon <JON@example.com>"]);
    }

    #[test]
    fn names_with_commas_are_quoted_so_the_line_parses() {
        let mut message = parent();
        message.headers.from = address(Some("Lovelace, \"Ada\""), "ada@x");
        let (to, _) = reply_recipients(&message, &me(), false);
        assert_eq!(to, vec!["\"Lovelace, Ada\" <ada@x>"]);
        assert_eq!(
            super::super::mailbox_parts(&to[0]),
            Some((Some("Lovelace, Ada".into()), "ada@x".into()))
        );
    }

    #[test]
    fn subjects_get_one_prefix() {
        assert_eq!(reply_subject("Plans"), "Re: Plans");
        assert_eq!(reply_subject("Re: Plans"), "Re: Plans");
        assert_eq!(reply_subject("RE: re: Fwd: Plans"), "Re: Plans");
        assert_eq!(reply_subject("AW: Plans"), "Re: Plans");
        assert_eq!(reply_subject("Re[2]: Plans"), "Re: Plans");
        assert_eq!(reply_subject("回复：计划"), "Re: 计划");
        assert_eq!(reply_subject("[team] Re: Plans"), "Re: [team] Plans");
        assert_eq!(reply_subject("[team] Plans"), "Re: [team] Plans");
        assert_eq!(reply_subject("Ready: launch"), "Re: Ready: launch");
        assert_eq!(reply_subject("  Re:  Two   spaces "), "Re: Two spaces");
        assert_eq!(reply_subject(""), "Re:");
        assert_eq!(forward_subject("Plans"), "Fwd: Plans");
        assert_eq!(forward_subject("Fwd: Plans"), "Fwd: Plans");
        assert_eq!(forward_subject("Re: Plans"), "Fwd: Plans");
    }

    #[test]
    fn references_are_the_parents_and_then_the_parent() {
        let refs = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        assert_eq!(
            build_references(&refs(&["<root@x>", "middle@x"]), "<parent@x>"),
            refs(&["root@x", "middle@x", "parent@x"])
        );
        assert_eq!(build_references(&[], "parent@x"), refs(&["parent@x"]));
        // Repeats go, and the parent comes last even when listed before.
        assert_eq!(
            build_references(
                &refs(&["root@x", "parent@x", "<root@x>", "mid@x", "mid@x"]),
                "parent@x"
            ),
            refs(&["root@x", "mid@x", "parent@x"])
        );
        // Ids that cannot be sent are dropped.
        let long = format!("{}@x", "a".repeat(300));
        assert_eq!(
            build_references(&refs(&["", "has space@x", &long, "ok@x"]), "parent@x"),
            refs(&["ok@x", "parent@x"])
        );
        // A long chain keeps its root and its newest ids.
        let chain: Vec<String> = (0..50).map(|n| format!("m{n}@x")).collect();
        let built = build_references(&chain, "parent@x");
        assert_eq!(built.len(), MAX_REFERENCES);
        assert_eq!(built[0], "m0@x");
        assert_eq!(built[1], "m32@x");
        assert_eq!(built.last().map(String::as_str), Some("parent@x"));
    }

    #[test]
    fn a_reply_starts_with_its_parent_and_the_quote() {
        let start = start_reply(ReplyKind::Reply, &parent(), "jon@example.com", &me());
        assert_eq!(start.fields.from, "jon@example.com");
        assert_eq!(start.fields.subject, "Re: Plans");
        assert_eq!(start.fields.in_reply_to.as_deref(), Some("parent@x"));
        assert_eq!(
            start.parts.references,
            vec!["root@x", "middle@x", "parent@x"]
        );
        assert!(
            start
                .parts
                .quote_text
                .starts_with("On Tue, 14 Nov 2023 22:13:20 +0000, Ada Lovelace <ada@x> wrote:\n"),
            "{}",
            start.parts.quote_text
        );
        assert!(
            start
                .parts
                .quote_text
                .ends_with("> See you at noon.\n>\n>> earlier\n"),
            "{}",
            start.parts.quote_text
        );
        assert!(
            start
                .parts
                .quote_html
                .contains("<blockquote><p>See you at noon.</p>")
        );
        assert!(start.parts.quote_html.contains("&gt; earlier"));
    }

    #[test]
    fn a_forward_has_no_recipients_and_names_the_attachments() {
        let mut message = parent();
        message.has_attachment = true;
        message.attachment_names = vec!["report.pdf".into(), "chart.png".into()];
        let start = start_reply(ReplyKind::Forward, &message, "jon@example.com", &me());
        assert!(start.fields.to.is_empty() && start.fields.cc.is_empty());
        assert_eq!(start.fields.subject, "Fwd: Plans");
        assert_eq!(start.fields.in_reply_to, None);
        assert!(start.parts.references.is_empty());
        let text = &start.parts.quote_text;
        assert!(
            text.starts_with(
                "---------- Forwarded message ----------\nFrom: Ada Lovelace <ada@x>\n"
            ),
            "{text}"
        );
        assert!(text.contains("\nSubject: Re: Plans\n"), "{text}");
        assert!(text.contains("\nTo: Jon <jon@example.com>, Maya <maya@x>\n"));
        assert!(text.contains("\nAttachments (not forwarded): report.pdf, chart.png\n"));
        assert!(
            text.ends_with("\n\nSee you at noon.\n\n> earlier\n"),
            "{text}"
        );
        assert!(start.parts.quote_html.contains("report.pdf, chart.png"));
    }

    fn stored(
        id: &str,
        in_reply_to: Option<&str>,
        references: &[&str],
        from: &str,
        date_unix: i64,
    ) -> Message {
        let mut message = parent();
        let message_id = MessageId::new(id);
        message.headers.message_id = message_id.clone();
        message.headers.in_reply_to = in_reply_to.map(MessageId::new);
        message.headers.references = references.iter().map(MessageId::new).collect();
        message.headers.from = address(None, from);
        message.headers.date_unix = date_unix;
        message.thread_id = ThreadId::from_message_id(&message_id);
        message.body_text = Some(format!("body of {id}"));
        message
    }

    #[test]
    fn a_sent_reply_parsed_back_threads_under_its_parent() {
        let account = crate::Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: crate::BackendKind::Bridge,
        };
        let root = stored("root@x", None, &[], "maya@x", 100);
        let middle = stored("middle@x", Some("root@x"), &["root@x"], "sam@x", 200);
        let parent = stored(
            "parent@x",
            Some("middle@x"),
            &["root@x", "middle@x"],
            "ada@x",
            300,
        );
        let store = crate::Store::open_in_memory().unwrap();
        store.upsert_account(&account).unwrap();
        for message in [&root, &middle, &parent] {
            store.upsert_message(message).unwrap();
        }
        let thread = store
            .message(&account.id, &parent.headers.message_id)
            .unwrap()
            .unwrap()
            .thread_id;

        // Reply as the inbox does: from the stored parent.
        let from_store = store
            .message(&account.id, &MessageId::new("<parent@x>"))
            .unwrap()
            .unwrap();
        let start = start_reply(
            ReplyKind::Reply,
            &from_store,
            &account.email,
            std::slice::from_ref(&account.email),
        );
        let outgoing = super::super::mime::build_outgoing(
            account.id.clone(),
            &start.fields,
            "Sounds good.\n",
            &[],
            &start.parts,
        );
        // The Sent copy, as sync fetches and stores it.
        let sent = crate::imap::to_message(
            &account.id,
            "Sent",
            "Sent",
            1,
            9,
            &["\\Seen".to_string()],
            &outgoing.mime,
        );
        assert_eq!(sent.headers.message_id, outgoing.message_id);
        assert_eq!(
            sent.headers.in_reply_to,
            Some(MessageId::new("parent@x")),
            "the send confirmation binds this parent"
        );
        assert_eq!(outgoing.draft.in_reply_to, Some(MessageId::new("parent@x")));
        let chain: Vec<&str> = sent
            .headers
            .references
            .iter()
            .map(MessageId::as_str)
            .collect();
        assert_eq!(chain, vec!["root@x", "middle@x", "parent@x"]);
        assert_eq!(sent.headers.subject, "Re: Plans");
        assert_eq!(store.upsert_message(&sent).unwrap(), thread);
        let shown = store.thread_messages(&account.id, &thread).unwrap();
        assert_eq!(shown.len(), 4);
        assert!(
            shown
                .iter()
                .any(|message| message.headers.message_id == outgoing.message_id)
        );

        // A mailbox that never got the parent or the middle message still
        // threads the reply with the root, through References.
        let other = crate::Store::open_in_memory().unwrap();
        other.upsert_account(&account).unwrap();
        let root_thread = other.upsert_message(&root).unwrap();
        assert_eq!(other.upsert_message(&sent).unwrap(), root_thread);
    }
}
