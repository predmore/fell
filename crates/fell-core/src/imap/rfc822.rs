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

pub(super) fn to_message(
    account_id: &AccountId,
    mailbox_raw: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    flags: &[String],
    raw: &[u8],
) -> Message {
    let parsed = MessageParser::new().parse(raw);
    let message_id = parsed
        .as_ref()
        .and_then(|message| message.message_id())
        .filter(|id| !id.is_empty())
        .map(MessageId::new)
        .unwrap_or_else(|| {
            MessageId::new(format!(
                "uid.{uidvalidity}.{uid}.{}@imap.fell",
                escape_mailbox(mailbox_raw)
            ))
        });
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
        references.insert(0, parent.clone());
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
    }
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

fn snippet(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = collapsed.len().min(160);
    while !collapsed.is_char_boundary(end) {
        end -= 1;
    }
    collapsed[..end].to_string()
}
