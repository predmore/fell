//! RFC 822 bytes from IMAP FETCH, turned into a store message.
//!
//! `body_text` is the plain text search and snippets use, with HTML reduced
//! to text. The HTML and plain parts are kept as sent in [`MessageParts`] for
//! the conversation view; sanitizing HTML belongs to rendering, not sync.

use mail_parser::{HeaderForm, MessageParser, PartKind};

use crate::account::AccountId;
use crate::message::{Address, Message, MessageHeaders, MessageId, MessageParts, RemoteId};
use crate::threading::ThreadId;

use super::escape_mailbox;

const PROTON_CONVERSATION_HEADERS: &[&str] = &[
    "X-Pm-Conversation-Id",
    "X-Pm-ConversationID",
    "X-PM-ConversationID",
    "X-Proton-Conversation-Id",
];

const ARRIVAL_HEADERS: &[&str] = &["Delivered-To", "X-Original-To", "Envelope-To"];

pub(super) fn is_deleted(flags: &[String]) -> bool {
    flags
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case("\\Deleted"))
}

/// The id [`to_message`] would store for this copy, from its headers alone.
pub(super) fn copy_message_id(
    mailbox_raw: &str,
    uidvalidity: u32,
    uid: u32,
    headers: &[u8],
) -> MessageId {
    let parsed = MessageParser::new().parse_headers(headers);
    message_id_or_uid(parsed.as_ref(), mailbox_raw, uidvalidity, uid)
}

/// The Message-ID header, or one made from the UID when there is none.
fn message_id_or_uid(
    parsed: Option<&mail_parser::Message<'_>>,
    mailbox_raw: &str,
    uidvalidity: u32,
    uid: u32,
) -> MessageId {
    parsed
        .and_then(|message| message.message_id())
        .filter(|id| !id.is_empty())
        .map(MessageId::new)
        .unwrap_or_else(|| {
            MessageId::new(format!(
                "uid.{uidvalidity}.{uid}.{}@imap.fell",
                escape_mailbox(mailbox_raw)
            ))
        })
}

pub(crate) fn to_message(
    account_id: &AccountId,
    mailbox_raw: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    flags: &[String],
    raw: &[u8],
) -> Message {
    let parsed = MessageParser::new().parse(raw);
    let message_id = message_id_or_uid(parsed.as_ref(), mailbox_raw, uidvalidity, uid);
    let in_reply_to = parsed.as_ref().and_then(|message| {
        message
            .in_reply_to()
            .and_then(|ids| ids.first())
            .filter(|id| !id.is_empty())
            .map(MessageId::new)
    });
    let mut references = parsed
        .as_ref()
        .and_then(|message| message.references())
        .map(|ids| {
            ids.iter()
                .filter(|id| !id.is_empty())
                .map(MessageId::new)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(parent) = &in_reply_to
        && !references.iter().any(|id| id == parent)
    {
        // The parent is the newest id, so it goes last: the first id stays
        // the thread's root, which a capped References keeps.
        references.push(parent.clone());
    }
    let from = parsed
        .as_ref()
        .and_then(|message| message.from())
        .and_then(|list| list.first());
    let subject = parsed
        .as_ref()
        .and_then(|message| message.subject())
        .unwrap_or("")
        .to_string();
    let date_unix = parsed
        .as_ref()
        .and_then(|message| message.date())
        .map(|date| date.to_timestamp())
        .unwrap_or(0);
    let proton_conversation_id = parsed.as_ref().and_then(proton_conversation);
    let body_text = parsed
        .as_ref()
        .and_then(|message| message.body_text(0))
        .map(|body| body.into_owned())
        .filter(|body| !body.trim().is_empty());
    let snippet = snippet(body_text.as_deref().unwrap_or(&subject));
    let unread = !flags.iter().any(|flag| flag.eq_ignore_ascii_case("\\Seen"));
    let labels = flags
        .iter()
        .filter(|flag| !flag.is_empty() && !flag.starts_with('\\'))
        .cloned()
        .collect();
    let arrived_via = parsed.as_ref().map(arrival_addresses).unwrap_or_default();
    let list_id = parsed.as_ref().and_then(list_id_header);
    let thread_id = ThreadId::from_message_id(&message_id);

    Message {
        account_id: account_id.clone(),
        headers: MessageHeaders {
            message_id,
            proton_conversation_id,
            in_reply_to,
            references,
            from: Address {
                name: from.and_then(|mailbox| mailbox.name()).map(str::to_string),
                email: from
                    .and_then(|mailbox| mailbox.address())
                    .unwrap_or("")
                    .to_string(),
            },
            subject,
            date_unix,
        },
        remote_id: RemoteId(format!(
            "imap:{uidvalidity}:{uid}:{}",
            escape_mailbox(mailbox_raw)
        )),
        thread_id,
        snippet,
        body_text,
        unread,
        folders: vec![folder.to_string()],
        labels,
        arrived_via,
        to: address_list(parsed.as_ref().and_then(|message| message.to())),
        cc: address_list(parsed.as_ref().and_then(|message| message.cc())),
        bcc: address_list(parsed.as_ref().and_then(|message| message.bcc())),
        starred: false,
        has_attachment: parsed
            .as_ref()
            .is_some_and(|message| message.has_attachments()),
        archived: false,
        snooze_until: None,
        list_id,
        reply_to: address_list(parsed.as_ref().and_then(|message| message.reply_to())),
        attachment_names: parsed.as_ref().map(attachment_names).unwrap_or_default(),
        parts: Some(parsed.as_ref().map(body_parts).unwrap_or_default()),
    }
}

/// Header names the renderer's bulk-mail check reads, matched exactly
/// (ASCII case-insensitive).
const BULK_HEADERS: &[&str] = &[
    "List-Unsubscribe",
    "List-Unsubscribe-Post",
    "List-Id",
    "Precedence",
    "Auto-Submitted",
    "X-Mailer",
    "Feedback-ID",
];

/// Header name prefixes email service providers stamp on bulk sends. Keep
/// this a superset of the ESP list in `fell-render`'s `choose.rs`.
const ESP_HEADER_PREFIXES: &[&str] = &[
    "x-mailchimp-",
    "x-mc-",
    "x-mailgun-",
    "x-ses-",
    "x-sendgrid-",
    "x-klaviyo-",
    "x-mailjet-",
    "x-mandrill-",
    "x-hs-",
    "x-hubspot-",
    "x-cmail-",
    "x-campaign",
    "x-sg-",
    "x-pm-message-id",
    "x-pm-tag",
    "x-pm-rcpt",
    "x-mailer-",
    "x-marketo",
    "x-csa-complaints",
];

/// A hostile message cannot make the kept headers large.
const MAX_BULK_HEADERS: usize = 32;
const MAX_BULK_HEADER_VALUE: usize = 2048;

/// [`body_parts`] of raw RFC 822 bytes. Bytes that do not parse have none.
pub(crate) fn parts_from_raw(raw: &[u8]) -> MessageParts {
    MessageParser::new()
        .parse(raw)
        .as_ref()
        .map(body_parts)
        .unwrap_or_default()
}

/// The HTML and plain-text body parts as sent, and the bulk-mail headers.
///
/// `text_body` and `html_body` (RFC 8621) list stand-ins for a missing
/// alternative: an HTML part in the text body of HTML-only mail, and the
/// reverse. Only parts of the matching kind are kept, so HTML-only mail has
/// no `plain` and plain-only mail has no `html`. Several parts of one kind
/// (text around an inline image) are joined in reading order.
fn body_parts(message: &mail_parser::Message<'_>) -> MessageParts {
    let html = joined(message.html_body(), |kind| matches!(kind, PartKind::Html));
    let plain = joined(message.text_body(), |kind| matches!(kind, PartKind::Text));
    MessageParts {
        html,
        plain,
        bulk_headers: bulk_headers(message),
    }
}

fn joined<'m>(
    parts: impl Iterator<Item = mail_parser::MessagePart<'m>>,
    wanted: impl Fn(&PartKind<'m>) -> bool,
) -> Option<String> {
    let mut out: Option<String> = None;
    for part in parts {
        if !wanted(&part.kind()) {
            continue;
        }
        let Some(text) = part.text() else {
            continue;
        };
        match &mut out {
            Some(joined) => {
                if !joined.ends_with('\n') {
                    joined.push('\n');
                }
                joined.push_str(&text);
            }
            None => out = Some(text.into_owned()),
        }
    }
    out.filter(|text| !text.trim().is_empty())
}

fn bulk_headers(message: &mail_parser::Message<'_>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for header in message.headers().iter() {
        if out.len() >= MAX_BULK_HEADERS {
            break;
        }
        let name = header.raw_name().trim();
        if !is_bulk_header(name) {
            continue;
        }
        let value = unfold(&String::from_utf8_lossy(header.raw_value()));
        if value.is_empty() {
            continue;
        }
        let mut end = value.len().min(MAX_BULK_HEADER_VALUE);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        out.push((name.to_string(), value[..end].to_string()));
    }
    out
}

fn is_bulk_header(name: &str) -> bool {
    if BULK_HEADERS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(name))
    {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    ESP_HEADER_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// One line: folded whitespace and control characters become single spaces.
fn unfold(raw: &str) -> String {
    raw.split(|ch: char| ch.is_whitespace() || ch.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// File names of the attachments, as a forward lists them. A part with no
/// name is called "attachment".
fn attachment_names(message: &mail_parser::Message<'_>) -> Vec<String> {
    message
        .attachments()
        .map(|part| {
            part.attachment_name()
                .map(|name| name.trim().replace(['\u{1e}', '\u{1f}'], " "))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "attachment".to_string())
        })
        .collect()
}

fn address_list(list: Option<mail_parser::AddressList<'_>>) -> Vec<Address> {
    let Some(list) = list else {
        return Vec::new();
    };
    list.mailboxes()
        .filter_map(|mailbox| {
            let email = mailbox.address()?.trim();
            if email.is_empty() {
                return None;
            }
            let name = mailbox
                .name()
                .map(str::to_string)
                .filter(|name| !name.is_empty());
            Some(Address {
                name,
                email: email.to_string(),
            })
        })
        .collect()
}

fn proton_conversation(message: &mail_parser::Message<'_>) -> Option<String> {
    let headers = message.headers();
    for name in PROTON_CONVERSATION_HEADERS {
        let text = headers
            .get(*name)
            .and_then(|header| header.value().as_text())
            .unwrap_or("")
            .trim()
            .to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn arrival_addresses(message: &mail_parser::Message<'_>) -> Vec<String> {
    let headers = message.headers();
    let mut out = Vec::new();
    for name in ARRIVAL_HEADERS {
        let Some(header) = headers.get(*name) else {
            continue;
        };
        let parsed = header.parse_as(HeaderForm::Addresses);
        if let Some(list) = parsed.value().as_address() {
            for mailbox in list.mailboxes() {
                if let Some(address) = mailbox.address()
                    && !address.is_empty()
                {
                    out.push(address.to_string());
                }
            }
            continue;
        }
        if let Some(text) = header.value().as_text() {
            let text = text.trim();
            if !text.is_empty() {
                out.push(text.to_string());
            }
        }
    }
    out
}

fn list_id_header(message: &mail_parser::Message<'_>) -> Option<String> {
    let address = message
        .list_id()?
        .mailboxes()
        .find_map(|mailbox| mailbox.address())
        .unwrap_or("")
        .trim();
    if address.is_empty() {
        None
    } else {
        Some(address.to_string())
    }
}

fn snippet(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = collapsed.len().min(160);
    while !collapsed.is_char_boundary(end) {
        end -= 1;
    }
    collapsed[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountId;

    #[test]
    fn list_id_is_the_angle_addr_and_cjk_subject_parses() {
        let raw = "From: Ann <ann@x>\r\n\
                   Subject: [list] 会議\r\n\
                   Message-ID: <a@x>\r\n\
                   List-ID: Dev list <dev.example>\r\n\
                   Date: Mon, 16 Nov 2026 12:00:00 +0000\r\n\
                   \r\n\
                   hello\r\n";
        let message = to_message(
            &AccountId("a1".into()),
            "INBOX",
            "INBOX",
            1,
            1,
            &[],
            raw.as_bytes(),
        );
        assert_eq!(message.list_id.as_deref(), Some("dev.example"));
        assert_eq!(message.headers.subject, "[list] 会議");
        assert_eq!(message.body_text.as_deref().map(str::trim), Some("hello"));
    }

    fn parts_of(raw: &str) -> (Message, MessageParts) {
        let message = to_message(
            &AccountId("a1".into()),
            "INBOX",
            "INBOX",
            1,
            9,
            &[],
            raw.as_bytes(),
        );
        let parts = message.parts.clone().expect("sync always parses parts");
        (message, parts)
    }

    fn is_bulk(parts: &MessageParts) -> bool {
        let mut headers = fell_render::MailHeaders::new();
        for (name, value) in &parts.bulk_headers {
            headers.add(name.as_str(), value.as_str());
        }
        headers.is_bulk()
    }

    #[test]
    fn multipart_alternative_keeps_both_parts() {
        let raw = "From: Ann <ann@x>\r\n\
                   Subject: Plans\r\n\
                   Message-ID: <alt@x>\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/alternative; boundary=\"b\"\r\n\
                   \r\n\
                   --b\r\n\
                   Content-Type: text/plain; charset=utf-8\r\n\
                   \r\n\
                   Lunch at noon?\r\n\
                   --b\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   Content-Transfer-Encoding: quoted-printable\r\n\
                   \r\n\
                   <p style=3D\"color:red\">Lunch at <b>noon</b>?</p>\r\n\
                   --b--\r\n";
        let (message, parts) = parts_of(raw);
        assert_eq!(
            parts.plain.as_deref().map(str::trim),
            Some("Lunch at noon?")
        );
        assert_eq!(
            parts.html.as_deref().map(str::trim),
            Some("<p style=\"color:red\">Lunch at <b>noon</b>?</p>")
        );
        assert!(parts.bulk_headers.is_empty());
        assert!(!is_bulk(&parts));
        assert_eq!(
            message.body_text.as_deref().map(str::trim),
            Some("Lunch at noon?")
        );
    }

    #[test]
    fn html_only_mail_has_no_plain_part_and_search_text_is_flattened() {
        let raw = "From: Ann <ann@x>\r\n\
                   Subject: Html\r\n\
                   Message-ID: <h@x>\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   \r\n\
                   <html><body><p>Hello <i>there</i></p><script>x()</script></body></html>\r\n";
        let (message, parts) = parts_of(raw);
        assert_eq!(parts.plain, None);
        let html = parts.html.expect("html part");
        // Stored as sent; sanitizing is the renderer's job.
        assert!(html.contains("<script>x()</script>"));
        let text = message.body_text.expect("flattened text");
        assert!(text.contains("Hello there"));
        assert!(!text.contains("<p>"));
    }

    #[test]
    fn plain_only_mail_has_no_html_part() {
        let raw = "From: Ann <ann@x>\r\n\
                   Subject: Plain\r\n\
                   Message-ID: <p@x>\r\n\
                   \r\n\
                   just text\r\n";
        let (_, parts) = parts_of(raw);
        assert_eq!(parts.html, None);
        assert_eq!(parts.plain.as_deref().map(str::trim), Some("just text"));
    }

    #[test]
    fn newsletter_keeps_bulk_headers_and_reads_as_bulk() {
        let raw = "From: Shop <news@shop.example>\r\n\
                   Subject: Sale\r\n\
                   Message-ID: <n@shop.example>\r\n\
                   List-Unsubscribe: <mailto:unsub@shop.example>,\r\n\
                   \t<https://shop.example/u?id=1>\r\n\
                   List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
                   Precedence: bulk\r\n\
                   X-Mailer: Mailchimp Mailer\r\n\
                   X-MC-User: 100\r\n\
                   X-Spam-Score: 0\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/alternative; boundary=\"b\"\r\n\
                   \r\n\
                   --b\r\n\
                   Content-Type: text/plain\r\n\
                   \r\n\
                   Sale today\r\n\
                   --b\r\n\
                   Content-Type: text/html\r\n\
                   \r\n\
                   <table><tr><td>Sale today</td></tr></table>\r\n\
                   --b--\r\n";
        let (_, parts) = parts_of(raw);
        assert!(parts.html.is_some() && parts.plain.is_some());
        assert_eq!(
            parts.bulk_headers,
            vec![
                (
                    "List-Unsubscribe".to_string(),
                    "<mailto:unsub@shop.example>, <https://shop.example/u?id=1>".to_string()
                ),
                (
                    "List-Unsubscribe-Post".to_string(),
                    "List-Unsubscribe=One-Click".to_string()
                ),
                ("Precedence".to_string(), "bulk".to_string()),
                ("X-Mailer".to_string(), "Mailchimp Mailer".to_string()),
                ("X-MC-User".to_string(), "100".to_string()),
            ]
        );
        assert!(is_bulk(&parts));
        // Each signal alone is enough for the renderer.
        for kept in &parts.bulk_headers[..] {
            let one = MessageParts {
                bulk_headers: vec![kept.clone()],
                ..MessageParts::default()
            };
            if kept.0 != "List-Unsubscribe-Post" {
                assert!(is_bulk(&one), "{} alone is bulk", kept.0);
            }
        }
    }

    #[test]
    fn every_renderer_esp_header_is_kept() {
        // The renderer's ESP names (fell-render choose.rs), one message each.
        for name in [
            "X-Mailchimp-Campaign",
            "X-Mailgun-Sid",
            "X-SES-Outgoing",
            "X-SendGrid-Id",
            "X-Klaviyo-Id",
            "X-Mailjet-Campaign",
            "X-Mandrill-User",
            "X-HS-Id",
            "X-HubSpot-Id",
            "X-CMail-Id",
            "X-Campaign-Id",
            "X-SG-EID",
            "X-SG-ID",
            "X-PM-Message-Id",
            "X-PM-Tag",
            "X-PM-Rcpt",
            "Feedback-ID",
        ] {
            let raw = format!("From: a@x\r\nMessage-ID: <e@x>\r\n{name}: v\r\n\r\nbody\r\n");
            let (_, parts) = parts_of(&raw);
            assert_eq!(parts.bulk_headers.len(), 1, "{name} kept");
            assert!(is_bulk(&parts), "{name} reads as bulk");
        }
    }

    #[test]
    fn personal_mail_with_an_ordinary_mailer_is_not_bulk() {
        let raw = "From: Ann <ann@x>\r\nMessage-ID: <t@x>\r\nX-Mailer: Thunderbird\r\n\r\nhi\r\n";
        let (_, parts) = parts_of(raw);
        assert_eq!(
            parts.bulk_headers,
            vec![("X-Mailer".to_string(), "Thunderbird".to_string())]
        );
        assert!(!is_bulk(&parts));
    }

    #[test]
    fn hostile_header_floods_are_capped() {
        let mut raw = String::from("From: a@x\r\nMessage-ID: <f@x>\r\n");
        for index in 0..100 {
            raw.push_str(&format!("X-MC-{index}: {}\r\n", "v".repeat(5000)));
        }
        raw.push_str("\r\nbody\r\n");
        let (_, parts) = parts_of(&raw);
        assert_eq!(parts.bulk_headers.len(), MAX_BULK_HEADERS);
        assert!(
            parts
                .bulk_headers
                .iter()
                .all(|(_, value)| value.len() <= MAX_BULK_HEADER_VALUE)
        );
    }

    #[test]
    fn reply_to_and_attachment_names_are_kept() {
        let raw = "From: Ann <ann@x>\r\n\
                   Reply-To: Plans list <plans@lists.x>, bob@x\r\n\
                   To: jon@example.com\r\n\
                   Subject: Report\r\n\
                   Message-ID: <r@x>\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/mixed; boundary=\"b\"\r\n\
                   \r\n\
                   --b\r\n\
                   Content-Type: text/plain\r\n\
                   \r\n\
                   see attached\r\n\
                   --b\r\n\
                   Content-Type: application/pdf; name=\"report.pdf\"\r\n\
                   Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   \r\n\
                   JVBERi0=\r\n\
                   --b\r\n\
                   Content-Type: application/octet-stream\r\n\
                   Content-Disposition: attachment\r\n\
                   \r\n\
                   xyz\r\n\
                   --b--\r\n";
        let message = to_message(
            &AccountId("a1".into()),
            "INBOX",
            "INBOX",
            1,
            2,
            &[],
            raw.as_bytes(),
        );
        assert_eq!(
            message.reply_to,
            vec![
                Address {
                    name: Some("Plans list".into()),
                    email: "plans@lists.x".into(),
                },
                Address {
                    name: None,
                    email: "bob@x".into(),
                },
            ]
        );
        assert!(message.has_attachment);
        assert_eq!(message.attachment_names, vec!["report.pdf", "attachment"]);
    }
}
