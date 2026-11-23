//! RFC 822 bytes from IMAP FETCH, turned into a store message.
//!
//! `body_text` is the plain text search uses, with HTML reduced to text.
//! The preview of HTML-only mail follows the native document's blocks.
//! The HTML and plain parts are kept as sent in [`MessageParts`] for the
//! conversation view; sanitizing HTML belongs to rendering, not sync.
//! Every other MIME part is listed by its IMAP section ([`MimePart`]); its
//! bytes are not kept.

use std::cell::OnceCell;
use std::collections::HashSet;
use std::ops::ControlFlow;

use mail_parser::{DateTime, HeaderForm, MessageParser, MessageRef, PartKind};

use crate::account::AccountId;
use crate::message::{
    Address, Disposition, Message, MessageHeaders, MessageId, MessageParts, MimePart, PartRole,
    RemoteId, TransferEncoding,
};
use crate::threading::ThreadId;

use super::{escape_mailbox, session};

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
    bridge: bool,
) -> MessageId {
    let parsed = MessageParser::new().parse_headers(headers);
    message_id_or_uid(parsed.as_ref(), mailbox_raw, uidvalidity, uid, bridge)
}

/// Whether there is no RFC Message-ID for write-back recovery to search for.
pub(super) fn is_synthetic_message_id(message_id: &str) -> bool {
    is_bridge_message_id(message_id)
        || (message_id.starts_with("uid.") && message_id.ends_with("@imap.fell"))
}

fn is_bridge_message_id(message_id: &str) -> bool {
    message_id.starts_with("bridge.") && message_id.ends_with("@imap.fell")
}

/// Bridge identifies the same message across mailboxes even when the RFC header is absent.
fn message_id_or_uid(
    parsed: Option<&mail_parser::Message<'_>>,
    mailbox_raw: &str,
    uidvalidity: u32,
    uid: u32,
    bridge: bool,
) -> MessageId {
    parsed
        .and_then(|message| message.message_id())
        .filter(|id| !id.is_empty())
        .map(MessageId::new)
        .or_else(|| {
            if !bridge {
                return None;
            }
            let id = parsed?
                .headers()
                .get("X-Pm-Internal-Id")?
                .value()
                .as_text()?
                .trim();
            // The Bridge identifier is case-sensitive and has URL-safe characters.
            (!id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_=".contains(&b)))
            .then(|| MessageId::new(format!("bridge.{id}@imap.fell")))
        })
        .unwrap_or_else(|| {
            MessageId::new(format!(
                "uid.{uidvalidity}.{uid}.{}@imap.fell",
                escape_mailbox(mailbox_raw)
            ))
        })
}

pub(super) fn keywords(flags: &[String]) -> Vec<String> {
    flags
        .iter()
        .filter(|flag| {
            !flag.is_empty() && !flag.starts_with('\\') && !flag.chars().any(char::is_control)
        })
        .cloned()
        .collect()
}

/// Read the draft's editable plain part and decoded files from the same MIME.
pub(crate) fn parse_draft(
    account: &crate::Account,
    id: &str,
    raw: &[u8],
) -> crate::Result<crate::compose::StoredCompose> {
    use crate::compose::{DraftFields, ReplyParts, ResolvedFile, StoredCompose};

    let parsed = MessageParser::new()
        .parse(raw)
        .ok_or_else(|| crate::Error::Action("Couldn't read the server draft".into()))?;
    let addresses = |list| {
        address_list(list)
            .into_iter()
            .map(|mut address| {
                address.name = address
                    .name
                    .map(|name| crate::notes::display_safe(&name, false));
                crate::compose::format_address(&address)
            })
            .collect()
    };
    let fields = DraftFields {
        from: account.compose_from(),
        to: addresses(parsed.to()),
        cc: addresses(parsed.cc()),
        bcc: addresses(parsed.bcc()),
        subject: crate::notes::display_safe(parsed.subject().unwrap_or_default(), false),
        in_reply_to: parsed
            .in_reply_to()
            .and_then(|ids| ids.first())
            .map(str::to_string),
        ..DraftFields::default()
    };
    let mut body = match joined(parsed.text_body(), |kind| matches!(kind, PartKind::Text)) {
        Some(plain) => plain.replace("\r\n", "\n"),
        None if parsed
            .html_body()
            .any(|part| matches!(part.kind(), PartKind::Html)) =>
        {
            let html = joined(parsed.html_body(), |kind| matches!(kind, PartKind::Html))
                .unwrap_or_default();
            let text = fell_render::html_to_document(&html).plain_text();
            if text.trim().is_empty() {
                return Err(crate::Error::Action(
                    "Server draft HTML has no editable text".into(),
                ));
            }
            text
        }
        None => String::new(),
    };
    let references = parsed
        .references()
        .map(|ids| ids.iter().map(str::to_string).collect())
        .unwrap_or_default();
    let paths = crate::compose::inline_image_paths(&body);
    let mut files = Vec::new();
    let mut bytes = 0_u64;
    for part in parsed.attachments() {
        let index = files.len() + 1;
        bytes += part.decoded_len() as u64;
        if index > crate::compose::MAX_ATTACHMENT_FILES
            || bytes > crate::compose::MAX_ATTACHMENT_BYTES
        {
            return Err(crate::Error::Action(
                "Server draft exceeds compose attachment limits".into(),
            ));
        }
        let mime = mime_type(&part);
        let name = crate::parts::sanitize_filename(part.attachment_name(), &mime);
        let inline_src = part.content_id().and_then(|cid| {
            paths.iter().find(|path| {
                crate::compose::draft_content_id(path) == cid
                    || path.strip_prefix("cid:") == Some(cid)
            })
        });
        let path = if let Some(src) = inline_src {
            let basename = name.replace(['(', ')'], "_");
            let path = format!("files/{index:02}-{basename}");
            body = body.replace(&format!("]({src})"), &format!("]({path})"));
            path
        } else {
            format!("files/{index:02}-{name}")
        };
        files.push(ResolvedFile {
            path,
            display_name: name,
            bytes: part.decoded().into_owned(),
            inline: inline_src.is_some(),
        });
    }
    Ok(StoredCompose {
        id: id.to_string(),
        account_id: account.id.clone(),
        fields,
        body,
        files,
        identities: vec![account.email.clone()],
        owner: String::new(),
        reply: ReplyParts {
            references,
            ..ReplyParts::default()
        },
    })
}

/// Parse the exact outgoing MIME without inventing a server copy or UID.
pub(crate) fn local_sent_message(account: &AccountId, folder: &str, mime: &[u8]) -> Message {
    let mut message = parse_message(
        account,
        (folder, folder),
        (0, 0),
        &["\\Seen".into()],
        mime,
        false,
    );
    message.remote_id = RemoteId(String::new());
    message
}

#[cfg(test)]
pub(crate) fn to_message(
    account_id: &AccountId,
    mailbox_raw: &str,
    folder: &str,
    uidvalidity: u32,
    uid: u32,
    flags: &[String],
    raw: &[u8],
) -> Message {
    parse_message(
        account_id,
        (mailbox_raw, folder),
        (uidvalidity, uid),
        flags,
        raw,
        false,
    )
}

pub(super) struct FetchedMessage {
    pub message: Message,
    pub deferred_date: Option<crate::backend::DeferredMailDate>,
}

pub(super) fn fetched_message(
    account_id: &AccountId,
    mailbox: &session::Listed,
    uidvalidity: u32,
    item: &session::Fetched,
    bridge: bool,
) -> FetchedMessage {
    let mut message = parse_message(
        account_id,
        (&mailbox.raw, &mailbox.decoded),
        (uidvalidity, item.uid),
        &item.flags,
        &item.body,
        bridge,
    );
    let latest = crate::latest_mail_date();
    let header = message.headers.date_unix;
    let arrival = item.internal_date;
    message.headers.date_unix = effective_date(header, arrival, latest);
    let deferred_date = if message.headers.date_unix == 0 {
        let plausible = |date: &i64| crate::plausible_future_mail_date(*date, latest);
        let header = Some(header).filter(plausible);
        let arrival = arrival.filter(plausible);
        (header.is_some() || arrival.is_some())
            .then_some(crate::backend::DeferredMailDate { header, arrival })
    } else {
        None
    };
    FetchedMessage {
        message,
        deferred_date,
    }
}

fn parse_message(
    account_id: &AccountId,
    (mailbox_raw, folder): (&str, &str),
    (uidvalidity, uid): (u32, u32),
    flags: &[String],
    raw: &[u8],
    bridge: bool,
) -> Message {
    let parsed = MessageParser::new().parse(raw);
    let message_id = message_id_or_uid(parsed.as_ref(), mailbox_raw, uidvalidity, uid, bridge);
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
        .and_then(|date| valid_date(&date))
        .unwrap_or(0);
    let proton_conversation_id = parsed.as_ref().and_then(proton_conversation);
    let body_text = parsed
        .as_ref()
        .and_then(|message| message.body_text(0))
        .map(|body| body.into_owned())
        .filter(|body| !body.trim().is_empty());
    let parts = parsed.as_ref().map(body_parts).unwrap_or_default();
    let snippet = html_only_snippet_text(&parts)
        .map(|text| cap_snippet(&text).to_owned())
        .unwrap_or_else(|| snippet(body_text.as_deref().unwrap_or(&subject)));
    let unread = !flags.iter().any(|flag| flag.eq_ignore_ascii_case("\\Seen"));
    let mut labels = keywords(flags);
    labels.retain(|keyword| !super::is_reserved_keyword(keyword));
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
        has_attachment: has_attachment(&parts.mime_parts),
        archived: false,
        snooze_until: None,
        list_id,
        reply_to: parsed
            .as_ref()
            .map(|message| address_list(message.reply_to())),
        attachment_names: attachment_names(&parts.mime_parts),
        parts: Some(parts),
    }
}

pub(super) fn valid_date(date: &DateTime) -> Option<i64> {
    if !date.is_valid() {
        return None;
    }
    // The parser permits days up to 31 before checking the month.
    let round_trip = DateTime::from_timestamp(date.to_timestamp_local());
    (round_trip.day == date.day).then(|| date.to_timestamp())
}

fn effective_date(header: i64, arrival: Option<i64>, latest: i64) -> i64 {
    [Some(header), arrival]
        .into_iter()
        .flatten()
        .find(|date| (1..=latest).contains(date))
        .unwrap_or(0)
}

/// A list row from a message's header alone, before its body is fetched.
///
/// It has no snippet, body or parts, so the store writes no body row for it
/// and the fetch of the whole message fills them in place. The attachment
/// marker waits for the body too: telling a file from an inline image needs
/// the HTML.
pub(super) fn headers_message(
    account_id: &AccountId,
    mailbox: &session::Listed,
    uidvalidity: u32,
    item: &session::Fetched,
    bridge: bool,
) -> FetchedMessage {
    let mut fetched = fetched_message(account_id, mailbox, uidvalidity, item, bridge);
    let message = &mut fetched.message;
    message.snippet.clear();
    message.body_text = None;
    message.parts = None;
    message.has_attachment = false;
    message.attachment_names.clear();
    fetched
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
    let mime_parts = list_parts(message.root(), html.as_deref());
    MessageParts {
        html,
        plain,
        bulk_headers: bulk_headers(message),
        mime_parts,
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

/// Whether any part is a file offered to the reader. Signatures and images
/// the HTML shows are not.
pub(crate) fn has_attachment(parts: &[MimePart]) -> bool {
    parts.iter().any(|part| part.role == PartRole::Attachment)
}

/// File names of the attachments, as a forward lists them: the message's
/// own, not those inside an attached message. A part with no name is called
/// "attachment".
pub(crate) fn attachment_names(parts: &[MimePart]) -> Vec<String> {
    parts
        .iter()
        .filter(|part| part.role == PartRole::Attachment && !part.nested)
        .map(|part| {
            part.filename
                .as_deref()
                .map(|name| name.replace(['\u{1e}', '\u{1f}'], " "))
                .unwrap_or_else(|| "attachment".to_string())
        })
        .collect()
}

/// A hostile message cannot make the part list large.
const MAX_MIME_PARTS: usize = 256;
const MAX_PART_NAME: usize = 255;
const MAX_MIME_TYPE: usize = 127;
/// Attached messages inside attached messages are listed this deep.
const MAX_NESTED_MESSAGES: usize = 4;

const SIGNATURE_TYPES: &[&str] = &[
    "application/pgp-signature",
    "application/pkcs7-signature",
    "application/x-pkcs7-signature",
];

/// The non-body parts of `message`, numbered as IMAP numbers them.
///
/// The top-level parts of a multipart message are `1`, `2`, …; a multipart
/// inside part `2` numbers its children `2.1`, `2.2`. A message that is not
/// multipart has one part, `1`. An attached `message/rfc822` at `3` is
/// listed itself, then its own parts follow under `3.`, as if it were the
/// whole message.
fn list_parts(message: MessageRef<'_>, html: Option<&str>) -> Vec<MimePart> {
    let mut out = Vec::new();
    walk_message(message, "", &CidReferences::new(html), 0, &mut out);
    out
}

fn walk_message(
    message: MessageRef<'_>,
    prefix: &str,
    cids: &CidReferences<'_>,
    depth: usize,
    out: &mut Vec<MimePart>,
) {
    let root = message.root_part();
    if root.is_multipart() {
        walk_children(root, prefix, cids, depth, out);
    } else {
        leaf(root, section(prefix, 1), cids, depth, out);
    }
}

fn walk_children(
    container: mail_parser::MessagePart<'_>,
    prefix: &str,
    cids: &CidReferences<'_>,
    depth: usize,
    out: &mut Vec<MimePart>,
) {
    for (index, child) in container.children().enumerate() {
        if out.len() >= MAX_MIME_PARTS {
            return;
        }
        let number = section(prefix, index + 1);
        if child.is_multipart() {
            walk_children(child, &number, cids, depth, out);
        } else {
            leaf(child, number, cids, depth, out);
        }
    }
}

fn section(prefix: &str, number: usize) -> String {
    if prefix.is_empty() {
        number.to_string()
    } else {
        format!("{prefix}.{number}")
    }
}

fn leaf(
    part: mail_parser::MessagePart<'_>,
    section: String,
    cids: &CidReferences<'_>,
    depth: usize,
    out: &mut Vec<MimePart>,
) {
    if out.len() >= MAX_MIME_PARTS {
        return;
    }
    let nested_message = part.nested();
    // The text the reader sees as the body is not a file.
    if nested_message.is_none() && part.is_text() && (part.in_text_body() || part.in_html_body()) {
        return;
    }
    let mime_type = mime_type(&part);
    let filename = part.attachment_name().and_then(clean_name);
    let size = part.decoded_len() as u64;
    // An empty text leftover in no body list (a stray preamble) is noise.
    if nested_message.is_none() && part.is_text() && filename.is_none() && size == 0 {
        return;
    }
    let content_id = part
        .content_id()
        .map(|id| {
            id.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .trim()
        })
        .filter(|id| !id.is_empty() && id.len() <= MAX_PART_NAME)
        .map(str::to_string);
    let disposition = match part.content_disposition() {
        Some(value) if value.ctype().eq_ignore_ascii_case("inline") => Disposition::Inline,
        Some(value) if value.ctype().eq_ignore_ascii_case("attachment") => Disposition::Attachment,
        _ => Disposition::Unspecified,
    };
    let encoding = match part.encoding() {
        mail_parser::Encoding::Base64 => TransferEncoding::Base64,
        mail_parser::Encoding::QuotedPrintable => TransferEncoding::QuotedPrintable,
        mail_parser::Encoding::None => TransferEncoding::Identity,
    };
    let mut listed = MimePart {
        section: section.clone(),
        role: PartRole::Attachment,
        mime_type,
        filename,
        size,
        encoded_size: part.raw_body().len() as u64,
        encoding,
        content_id,
        disposition,
        nested: depth > 0,
    };
    listed.role = cids.role_for(&listed);
    out.push(listed);
    if let Some(inner) = nested_message
        && depth < MAX_NESTED_MESSAGES
    {
        let html = joined(inner.html_body(), |kind| matches!(kind, PartKind::Html));
        let cids = CidReferences::new(html.as_deref());
        walk_message(inner, &section, &cids, depth + 1, out);
    }
}

/// `smime.p7s` and the like, or PGP's `signature.asc`.
fn is_signature_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".p7s") || lower == "signature.asc"
}

fn mime_type(part: &mail_parser::MessagePart<'_>) -> String {
    let given = part.content_type().and_then(|value| {
        let subtype = value.subtype()?;
        let joined = format!("{}/{}", value.ctype(), subtype).to_ascii_lowercase();
        let token = |ch: char| ch.is_ascii_alphanumeric() || "!#$&^_.+-/".contains(ch);
        (joined.len() <= MAX_MIME_TYPE && joined.chars().all(token)).then_some(joined)
    });
    given.unwrap_or_else(|| {
        if part.is_message() {
            "message/rfc822"
        } else if part.content_type().is_none() && part.is_text() {
            "text/plain"
        } else {
            "application/octet-stream"
        }
        .to_string()
    })
}

/// One line, no control or bidi-override characters, at most 255 bytes.
fn clean_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .filter(|ch| !is_bidi_control(*ch))
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = cleaned.len().min(MAX_PART_NAME);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    let cleaned = cleaned[..end].trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

pub(crate) fn is_bidi_control(ch: char) -> bool {
    matches!(ch, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Parse each HTML body only when a part could be an inline image.
pub(crate) struct CidReferences<'a> {
    html: Option<&'a str>,
    ids: OnceCell<HashSet<String>>,
}

impl<'a> CidReferences<'a> {
    pub(crate) fn new(html: Option<&'a str>) -> Self {
        Self {
            html,
            ids: OnceCell::new(),
        }
    }

    /// Files stay visible unless they are signatures or the HTML loads them.
    pub(crate) fn role_for(&self, part: &MimePart) -> PartRole {
        if SIGNATURE_TYPES.contains(&part.mime_type.as_str())
            || part.filename.as_deref().is_some_and(is_signature_name)
        {
            PartRole::Signature
        } else if part.cid_image_id().is_some_and(|id| self.contains(id)) {
            PartRole::Inline
        } else {
            PartRole::Attachment
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.html.is_some_and(|html| {
            self.ids
                .get_or_init(|| cid_references(html))
                .contains(&id.to_ascii_lowercase())
        })
    }
}

/// Normalize CID URL payloads (RFC 2392) for comparison with MIME headers.
fn cid_references(html: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let _ = fell_render::visit_cid_image_urls(html, |payload| {
        // Each decoded byte can occupy up to three bytes in the URL.
        if payload.len() <= 3 * MAX_PART_NAME {
            let id = percent_decode(payload);
            if id.len() <= MAX_PART_NAME {
                out.insert(id);
            }
        }
        if out.len() >= MAX_MIME_PARTS {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = text.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_ascii_lowercase()
}

/// An empty header-fields response has only the terminating blank line.
/// Missing or incomplete bytes cannot establish that Reply-To is absent.
pub(super) fn reply_to_from_header(raw: &[u8]) -> Option<Vec<Address>> {
    if raw == b"\r\n" {
        return Some(Vec::new());
    }
    if !raw.ends_with(b"\r\n\r\n") {
        return None;
    }
    let parsed = MessageParser::new().parse_headers(raw)?;
    parsed
        .headers()
        .iter()
        .any(|header| header.raw_name().eq_ignore_ascii_case("Reply-To"))
        .then(|| address_list(parsed.reply_to()))
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

pub(crate) fn html_only_snippet_text(parts: &MessageParts) -> Option<String> {
    parts
        .html
        .as_deref()
        .filter(|_| parts.plain.is_none())
        .and_then(html_snippet_text)
}

pub(crate) fn html_snippet_text(html: &str) -> Option<String> {
    let preview = collapse_snippet(&fell_render::html_to_document(html).plain_text());
    (!preview.is_empty()).then_some(preview)
}

pub(crate) const SNIPPET_BYTES: usize = 160;

pub(crate) fn cap_snippet(text: &str) -> &str {
    let mut end = text.len().min(SNIPPET_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn collapse_snippet(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn snippet(text: &str) -> String {
    cap_snippet(&collapse_snippet(text)).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountId;

    #[test]
    fn dates_allow_one_day_of_clock_skew() {
        let now = 1_707_519_600;
        let latest = now + 86_400;
        assert_eq!(effective_date(latest, Some(now), latest), latest);
        assert_eq!(effective_date(latest + 1, Some(now), latest), now);
        assert_eq!(effective_date(0, Some(latest + 1), latest), 0);
    }

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
    fn html_snippet_separates_blocks_and_keeps_inline_words_together() {
        let raw = "From: Ann <ann@x>\r\n\
                   Subject: Survey\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\r\n\
                   <h1>From <em>the</em> field</h1><p>The latest <b>sur</b>vey.</p>\
                   <ul><li>First route</li><li>Second route</li></ul>";
        let (message, _) = parts_of(raw);
        assert_eq!(
            message.snippet,
            "From the field The latest survey. First route Second route"
        );
    }

    #[test]
    fn cached_html_snippets_only_repair_block_spacing() {
        let paragraph = "The survey continues along the marked route. ".repeat(6);
        let long_heading = format!("<h1>Heading</h1><p>{paragraph}</p>");
        let long_preheader = format!(
            "<div style=\"display:none\">Preheader</div><h1>Heading</h1><p>{paragraph}</p>"
        );
        let old_long = snippet(&format!("Heading{paragraph}"));
        let fixed_long = snippet(&format!("Heading {paragraph}"));
        assert_eq!(old_long.len(), SNIPPET_BYTES);
        assert_eq!(fixed_long.len(), SNIPPET_BYTES);
        assert!(
            fixed_long.chars().filter(|ch| !ch.is_whitespace()).count()
                < old_long.chars().filter(|ch| !ch.is_whitespace()).count()
        );
        let old_preheader = snippet(&format!("PreheaderHeading{paragraph}"));
        let unicode = format!("{}🧭Follow the signs.", "a".repeat(SNIPPET_BYTES - 10));
        let unicode_heading = format!("<h1>Heading</h1><p>{unicode}</p>");
        let old_unicode = snippet(&format!("Heading{unicode}"));
        let fixed_unicode = snippet(&format!("Heading {unicode}"));
        assert_eq!(old_unicode.len(), SNIPPET_BYTES - 3);
        let cases = [
            (
                "<div style=\"display:none\">Preheader</div><p>News today.</p>",
                "PreheaderNews today.",
                "PreheaderNews today.",
            ),
            (
                "<img src=\"logo.png\" alt=\"Company logo\"><p>News today.</p>",
                "News today.",
                "News today.",
            ),
            (
                "<h1>From the field</h1><p>The latest survey.</p>",
                "From the fieldThe latest survey.",
                "From the field The latest survey.",
            ),
            ("<p>News today.</p>", "News today.", "News today."),
            ("<b>sur</b>vey", "survey", "survey"),
            (&long_heading, &old_long, &fixed_long),
            (&long_preheader, &old_preheader, &old_preheader),
            (&unicode_heading, &old_unicode, &fixed_unicode),
        ];
        let store = crate::Store::open_in_memory().unwrap();
        let account = AccountId("a1".into());
        store
            .upsert_account(&crate::Account {
                id: account.clone(),
                display_name: "Reader".into(),
                email: "reader@example.test".into(),
                backend_kind: crate::BackendKind::Imap,
            })
            .unwrap();
        let messages: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(i, (html, old, _))| {
                let raw = format!(
                    "From: Ann <ann@x>\r\n\
                     Subject: Example {i}\r\n\
                     Message-ID: <{i}@x>\r\n\
                     Content-Type: text/html; charset=utf-8\r\n\r\n{html}"
                );
                let mut message = to_message(
                    &account,
                    "INBOX",
                    "INBOX",
                    1,
                    i as u32 + 1,
                    &[],
                    raw.as_bytes(),
                );
                // Match the snippet builder used before the block-spacing change.
                assert_eq!(snippet(message.body_text.as_deref().unwrap()), *old);
                message.snippet = (*old).into();
                message
            })
            .collect();
        store
            .apply_delta(
                &account,
                &crate::MailboxDelta {
                    messages: messages.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('html_snippet_block_spacing', -1)",
                [],
            )
            .unwrap();

        assert_eq!(store.repair_html_snippets().unwrap(), Some(true));
        assert_eq!(store.repair_html_snippets().unwrap(), None);
        for (message, (_, _, expected)) in messages.iter().zip(cases) {
            let stored = store
                .message(&account, &message.headers.message_id)
                .unwrap()
                .unwrap();
            assert_eq!(stored.snippet, expected);
            assert_eq!(stored.body_text, message.body_text);
        }
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
            Some(vec![
                Address {
                    name: Some("Plans list".into()),
                    email: "plans@lists.x".into(),
                },
                Address {
                    name: None,
                    email: "bob@x".into(),
                },
            ])
        );
        assert!(message.has_attachment);
        assert_eq!(message.attachment_names, vec!["report.pdf", "attachment"]);
    }

    use crate::imap::mime_fixtures::{self, MimeFixture};
    use crate::message::{Disposition, MimePart, PartRole, TransferEncoding};

    fn listed(fixture: &MimeFixture) -> (Message, Vec<MimePart>) {
        let message = to_message(
            &AccountId("a1".into()),
            "INBOX",
            "INBOX",
            1,
            3,
            &[],
            &fixture.raw,
        );
        let parts = message.parts.clone().unwrap().mime_parts;
        (message, parts)
    }

    /// Each listed section, read with the test server's own MIME walk and
    /// decoded, is the fixture's bytes.
    fn assert_sections_hold_the_bytes(fixture: &MimeFixture, parts: &[MimePart]) {
        for (section, bytes) in &fixture.bytes {
            let part = parts
                .iter()
                .find(|part| part.section == *section)
                .unwrap_or_else(|| panic!("section {section} is listed: {parts:?}"));
            let raw = super::super::test_server::section_body(&fixture.raw, section)
                .unwrap_or_else(|| panic!("the server finds section {section}"));
            assert_eq!(part.encoded_size, raw.len() as u64, "section {section}");
            assert_eq!(
                &crate::parts::decode_part(part.encoding, &raw),
                bytes,
                "section {section}"
            );
            assert_eq!(part.size, bytes.len() as u64, "section {section}");
        }
    }

    #[test]
    fn latin1_text_attachments_are_sized_by_their_transfer_decoded_bytes() {
        let fixture = mime_fixtures::latin1_text_attachments("l1@x");
        let (_, parts) = listed(&fixture);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_sections_hold_the_bytes(&fixture, &parts);
    }

    #[test]
    fn mixed_mail_lists_both_attachments_by_section() {
        let fixture = mime_fixtures::mixed_two_attachments("mix@x");
        let (message, parts) = listed(&fixture);
        assert_eq!(parts.len(), 2, "{parts:?}");
        let pdf = &parts[0];
        assert_eq!(pdf.section, "2");
        assert_eq!(pdf.role, PartRole::Attachment);
        assert_eq!(pdf.mime_type, "application/pdf");
        assert_eq!(pdf.filename.as_deref(), Some("report.pdf"));
        assert_eq!(pdf.encoding, TransferEncoding::Base64);
        assert_eq!(pdf.disposition, Disposition::Attachment);
        assert!(!pdf.nested);
        let csv = &parts[1];
        assert_eq!(csv.section, "3");
        assert_eq!(csv.mime_type, "text/csv");
        assert_eq!(csv.filename.as_deref(), Some("totals.csv"));
        assert_eq!(csv.encoding, TransferEncoding::QuotedPrintable);
        assert_sections_hold_the_bytes(&fixture, &parts);
        assert!(message.has_attachment);
        assert_eq!(message.attachment_names, vec!["report.pdf", "totals.csv"]);
        // The text body is the body, not a file.
        assert_eq!(
            message.body_text.as_deref().map(str::trim),
            Some("Two files attached.")
        );
    }

    #[test]
    fn related_images_the_html_shows_are_inline_not_attachments() {
        let fixture = mime_fixtures::related_cid_images("rel@x");
        let (message, parts) = listed(&fixture);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].section, "2");
        assert_eq!(parts[0].role, PartRole::Inline);
        assert_eq!(parts[0].content_id.as_deref(), Some("logo@x"));
        assert_eq!(parts[0].mime_type, "image/png");
        // An image no cid: names is something the reader can only open.
        assert_eq!(parts[1].section, "3");
        assert_eq!(parts[1].role, PartRole::Attachment);
        assert_eq!(parts[1].filename.as_deref(), Some("stray.png"));
        assert_sections_hold_the_bytes(&fixture, &parts);
        assert!(message.has_attachment);
        assert_eq!(message.attachment_names, vec!["stray.png"]);

        // With only the logo, the message has no attachment.
        let only_logo = String::from_utf8(fixture.raw.clone()).unwrap();
        let cut = only_logo
            .find("--rel\r\nContent-Type: image/png; name=")
            .unwrap();
        let only_logo = format!("{}--rel--\r\n", &only_logo[..cut]);
        let (message, parts) = listed(&MimeFixture {
            raw: only_logo.into_bytes(),
            bytes: Vec::new(),
        });
        assert_eq!(parts.len(), 1);
        assert!(!message.has_attachment);
        assert!(message.attachment_names.is_empty());
    }

    #[test]
    fn an_rfc_2231_name_is_decoded() {
        let fixture = mime_fixtures::rfc2231_name("2231@x");
        let (message, parts) = listed(&fixture);
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert_eq!(parts[0].filename.as_deref(), Some("Ñandú 日本語.txt"));
        assert_eq!(parts[0].mime_type, "application/octet-stream");
        assert_sections_hold_the_bytes(&fixture, &parts);
        assert_eq!(message.attachment_names, vec!["Ñandú 日本語.txt"]);
    }

    #[test]
    fn an_attached_message_is_listed_then_its_own_parts() {
        let fixture = mime_fixtures::nested_message("fwd@x");
        let (message, parts) = listed(&fixture);
        let sections: Vec<_> = parts.iter().map(|part| part.section.as_str()).collect();
        assert_eq!(sections, ["2", "2.2"], "{parts:?}");
        assert_eq!(parts[0].mime_type, "message/rfc822");
        assert_eq!(parts[0].filename.as_deref(), Some("Original.eml"));
        assert_eq!(parts[0].role, PartRole::Attachment);
        assert!(!parts[0].nested);
        assert_eq!(parts[1].mime_type, "application/zip");
        assert_eq!(parts[1].filename.as_deref(), Some("inner.zip"));
        assert!(parts[1].nested);
        assert_sections_hold_the_bytes(&fixture, &parts);
        // A forward names the attached message, not the files inside it.
        assert_eq!(message.attachment_names, vec!["Original.eml"]);
    }

    #[test]
    fn a_signature_is_not_an_attachment() {
        let fixture = mime_fixtures::pgp_signed("sig@x");
        let (message, parts) = listed(&fixture);
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert_eq!(parts[0].role, PartRole::Signature);
        assert_eq!(parts[0].filename.as_deref(), Some("signature.asc"));
        assert_sections_hold_the_bytes(&fixture, &parts);
        assert!(!message.has_attachment, "issue #148");
        assert!(message.attachment_names.is_empty());

        // S/MIME sent as octet-stream: known by its `.p7s` name.
        let smime = String::from_utf8(fixture.raw)
            .unwrap()
            .replace(
                "application/pgp-signature; name=\"signature.asc\"",
                "application/octet-stream",
            )
            .replace("filename=\"signature.asc\"", "filename=\"smime.p7s\"");
        let (message, parts) = listed(&MimeFixture {
            raw: smime.into_bytes(),
            bytes: Vec::new(),
        });
        assert_eq!(parts[0].role, PartRole::Signature);
        assert!(!message.has_attachment);
    }

    /// A related message: HTML `html`, then one part with `part_headers`.
    fn related_with(html: &str, part_headers: &str) -> (Message, Vec<MimePart>) {
        let raw = format!(
            "From: a@x\r\nMessage-ID: <rel2@x>\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/related; boundary=\"rel\"\r\n\r\n\
             --rel\r\nContent-Type: text/html\r\n\r\n{html}\r\n\
             --rel\r\n{part_headers}\r\nContent-ID: <part@x>\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{}\r\n--rel--\r\n",
            mime_fixtures::base64(b"%PDF or PNG")
        );
        listed(&MimeFixture {
            raw: raw.into_bytes(),
            bytes: Vec::new(),
        })
    }

    #[test]
    fn only_an_image_the_html_loads_is_inline() {
        let png = "Content-Type: image/png; name=\"logo.png\"";
        let shown = [
            "<img src=\"cid:part@x\">",
            "<img SRC = 'cid:part@x'>",
            "<img src=' CID&#58;PART%40x '>",
            "<body background=cid:part@x>",
            "<table><tr><td background=cid:part@x>Logo</td></tr></table>",
            "<video poster=cid:part@x></video>",
            "<div style=\"background: url(cid:part@x)\">",
            "<div style=\"background: URL( &quot;cid:part@x&quot; )\">",
            r"<style>@media screen { .logo { background: image-set(url(\63 id:part@x) 1x) } }</style>",
            "<noscript><img src=cid:part@x></noscript>",
        ];
        for html in shown {
            let (message, parts) = related_with(html, png);
            assert_eq!(parts[0].role, PartRole::Inline, "{html}");
            assert!(!message.has_attachment, "{html}");
        }
        // An image sent as an attachment stays one, even when the HTML shows it.
        let (message, parts) = related_with(
            "<img src=\"cid:part@x\">",
            "Content-Type: image/png\r\nContent-Disposition: attachment; filename=\"photo.png\"",
        );
        assert_eq!(parts[0].role, PartRole::Attachment);
        assert!(message.has_attachment);
        // Something other than an image is never hidden as inline.
        let (message, parts) = related_with(
            "<img src=\"cid:part@x\">",
            "Content-Type: application/pdf; name=\"invoice.pdf\"",
        );
        assert_eq!(parts[0].role, PartRole::Attachment);
        assert_eq!(message.attachment_names, vec!["invoice.pdf"]);
    }

    #[test]
    fn cid_mentions_without_an_image_reference_leave_the_attachment() {
        let png = "Content-Type: image/png; name=\"logo.png\"";
        for html in [
            "<p>see cid:part@x</p>",
            "<p>Write src=cid:part@x in the message</p>",
            "<p>Write background=cid:part@x in the message</p>",
            "<p>Write poster=cid:part@x in the message</p>",
            "<p>Write url(cid:part@x) in the message</p>",
            "<code>&lt;img src=cid:part@x&gt;</code>",
            "<!-- <img src=cid:part@x> -->",
            "<script>const example = '<img src=cid:part@x>';</script>",
            "<template><img src=cid:part@x></template>",
            "<a href=\"cid:part@x\">logo</a>",
            "<img data-src=\"cid:part@x\">",
            "<img alt=\"cid:part@x\">",
            "<img alt=\"Write src=cid:part@x in the message\">",
            "<img src=\"cid:part@x extra\">",
            "<video src=cid:part@x></video>",
            "<div background=cid:part@x poster=cid:part@x></div>",
            "<div data-style=\"background: url(cid:part@x)\"></div>",
            "<div style=\"Write url(cid:part@x) here\"></div>",
            "<div style=\"/* background: url(cid:part@x) */\"></div>",
            "<style>p::before { content: 'url(cid:part@x)' }</style>",
            "<style>p { background: not-url(cid:part@x) }</style>",
            "<style>@import url(cid:part@x);</style>",
            "<style>@font-face { font-family: Example; src: url(cid:part@x) }</style>",
            "<style>@supports (background: url(cid:part@x)) { p { color: red } }</style>",
            "<style type=text/plain>p { background: url(cid:part@x) }</style>",
        ] {
            let (message, parts) = related_with(html, png);
            assert_eq!(parts[0].role, PartRole::Attachment, "{html}");
            assert!(message.has_attachment, "{html}");
            assert_eq!(message.attachment_names, vec!["logo.png"], "{html}");
        }
        let (message, parts) = related_with(
            "<p>Write src=cid:part@x in the message</p>",
            "Content-Type: image/png\r\nContent-Disposition: inline; filename=\"photo.png\"",
        );
        assert_eq!(parts[0].role, PartRole::Attachment);
        assert_eq!(message.attachment_names, vec!["photo.png"]);
    }

    #[test]
    fn the_second_part_of_signed_mail_is_a_signature_only_when_it_is_one() {
        let signed = |headers: &str| {
            let raw = format!(
                "From: a@x\r\nMessage-ID: <s2@x>\r\nMIME-Version: 1.0\r\n\
                 Content-Type: multipart/signed; boundary=\"sig\"; protocol=\"x\"\r\n\r\n\
                 --sig\r\nContent-Type: text/plain\r\n\r\nsigned text\r\n\
                 --sig\r\n{headers}\r\n\r\nMIIB\r\n--sig--\r\n"
            );
            listed(&MimeFixture {
                raw: raw.into_bytes(),
                bytes: Vec::new(),
            })
        };
        let (message, parts) = signed(
            "Content-Type: application/octet-stream; name=\"invoice.exe\"\r\n\
             Content-Disposition: attachment; filename=\"invoice.exe\"",
        );
        assert_eq!(parts[0].role, PartRole::Attachment);
        assert_eq!(message.attachment_names, vec!["invoice.exe"]);
        for headers in [
            "Content-Type: application/pkcs7-signature",
            "Content-Type: application/x-pkcs7-signature",
            "Content-Type: application/octet-stream; name=\"SMIME.P7S\"",
            "Content-Type: text/plain; name=\"signature.asc\"",
        ] {
            let (message, parts) = signed(headers);
            assert_eq!(parts[0].role, PartRole::Signature, "{headers}");
            assert!(!message.has_attachment, "{headers}");
        }
    }

    #[test]
    fn a_message_that_is_one_file_is_part_one() {
        let raw = format!(
            "From: a@x\r\nMessage-ID: <one@x>\r\nMIME-Version: 1.0\r\n\
             Content-Type: application/pdf; name=\"scan.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            mime_fixtures::base64(b"%PDF scan")
        );
        let fixture = MimeFixture {
            raw: raw.into_bytes(),
            bytes: vec![("1", b"%PDF scan".to_vec())],
        };
        let (message, parts) = listed(&fixture);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].section, "1");
        assert_sections_hold_the_bytes(&fixture, &parts);
        assert!(message.has_attachment);
    }

    #[test]
    fn hostile_part_lists_and_names_are_capped_and_cleaned() {
        let mut raw = String::from(
            "From: a@x\r\nMessage-ID: <flood@x>\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"f\"\r\n\r\n",
        );
        raw.push_str(
            "--f\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=\"../../evil\u{202e}fdp.exe\x07\"\r\n\r\nx\r\n",
        );
        for index in 0..400 {
            raw.push_str(&format!(
                "--f\r\nContent-Type: application/x-{index}\r\n\
                 Content-Disposition: attachment\r\n\r\nx\r\n"
            ));
        }
        raw.push_str("--f--\r\n");
        let (_, parts) = listed(&MimeFixture {
            raw: raw.into_bytes(),
            bytes: Vec::new(),
        });
        assert_eq!(parts.len(), MAX_MIME_PARTS);
        // Kept as sent apart from control and bidi characters; making it a
        // safe file name is the cache's job.
        assert_eq!(parts[0].filename.as_deref(), Some("../../evilfdp.exe"));
        assert_eq!(
            crate::parts::sanitize_filename(parts[0].filename.as_deref(), &parts[0].mime_type),
            "_.._evilfdp.exe"
        );
    }
}
