//! RFC 822 bytes from IMAP FETCH, turned into a store message.
//!
//! HTML is reduced to plain text for the local body. Rendering HTML is later
//! work and does not belong in sync.

use mail_parser::{HeaderForm, MessageParser};

use crate::account::AccountId;
use crate::message::{Address, Message, MessageHeaders, MessageId, RemoteId};
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
    }
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
