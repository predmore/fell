//! Multipart message built from the markdown the editor is showing.
//!
//! `text/plain` is that markdown. `text/html` comes from the shared renderer.
//! Inline images are CID parts. Remote URLs are not fetched.
//!
//! The bytes in [`Outgoing::mime`] are the message that leaves the machine.
//! `Bcc` stays on [`crate::send::Draft`] for the envelope and is not a header
//! of that message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::send::Draft;

use super::frontmatter::{DraftFields, mailbox_parts};
use super::markdown::{doc_to_html, markdown_to_doc};

static TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFile {
    pub path: String,
    pub display_name: String,
    pub bytes: Vec<u8>,
    pub inline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub draft: Draft,
    /// The `Message-ID` header in [`Self::mime`]. The Sent copy the server
    /// files carries it, so a local record of the Bcc can find that copy.
    pub message_id: crate::MessageId,
    pub mime: Vec<u8>,
    pub plain: String,
    pub html: String,
}

pub fn build_outgoing(
    account_id: crate::AccountId,
    fields: &DraftFields,
    body: &str,
    files: &[ResolvedFile],
) -> Outgoing {
    let plain = if body.ends_with('\n') || body.is_empty() {
        body.to_string()
    } else {
        format!("{body}\n")
    };
    let html = doc_to_html(&markdown_to_doc(&plain));
    let digest = attachment_digest(files);
    let message_id = fresh_message_id();
    let mime = render_mime(fields, &message_id, &plain, &html, files);
    let in_reply_to = fields
        .in_reply_to
        .as_ref()
        .filter(|value| !value.is_empty())
        .map(crate::MessageId::new);
    Outgoing {
        draft: Draft {
            account_id,
            to: fields.to.clone(),
            cc: fields.cc.clone(),
            bcc: fields.bcc.clone(),
            subject: fields.subject.clone(),
            body: plain.clone(),
            in_reply_to,
            attachments_digest: digest,
        },
        message_id: crate::MessageId::new(&message_id),
        mime,
        plain,
        html,
    }
}

fn render_mime(
    fields: &DraftFields,
    message_id: &str,
    plain: &str,
    html: &str,
    files: &[ResolvedFile],
) -> Vec<u8> {
    let blobs = content_blobs(plain, html, files);
    let mixed = fresh_boundary("mix", &blobs);
    let related = fresh_boundary("rel", &blobs);
    let alternative = fresh_boundary("alt", &blobs);
    let inline: Vec<_> = files.iter().filter(|file| file.inline).collect();
    let attached: Vec<_> = files.iter().filter(|file| !file.inline).collect();
    let mut headers = String::new();
    headers.push_str(&header_line("From", &encode_address(&fields.from)));
    headers.push_str(&header_line("To", &encode_addresses(&fields.to)));
    if !fields.cc.is_empty() {
        headers.push_str(&header_line("Cc", &encode_addresses(&fields.cc)));
    }
    headers.push_str(&header_line(
        "Subject",
        &encode_unstructured(&fields.subject),
    ));
    headers.push_str(&header_line("Date", &rfc5322_now()));
    headers.push_str(&header_line("Message-ID", &angle_id(message_id)));
    if let Some(reply) = fields
        .in_reply_to
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        let id = angle_id(reply);
        headers.push_str(&header_line("In-Reply-To", &id));
        headers.push_str(&header_line("References", &id));
    }
    headers.push_str("MIME-Version: 1.0\r\n");
    let mut body = String::new();
    let alternative_part = alternative_part(&alternative, plain, html);
    if inline.is_empty() && attached.is_empty() {
        headers.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{alternative}\"\r\n\r\n"
        ));
        body.push_str(&alternative_part);
    } else if attached.is_empty() {
        headers.push_str(&format!(
            "Content-Type: multipart/related; boundary=\"{related}\"\r\n\r\n"
        ));
        body.push_str(&format!("--{related}\r\n"));
        body.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{alternative}\"\r\n\r\n"
        ));
        body.push_str(&alternative_part);
        for file in inline {
            body.push_str(&format!("--{related}\r\n"));
            body.push_str(&inline_part(file));
        }
        body.push_str(&format!("--{related}--\r\n"));
    } else {
        headers.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{mixed}\"\r\n\r\n"
        ));
        body.push_str(&format!("--{mixed}\r\n"));
        if inline.is_empty() {
            body.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{alternative}\"\r\n\r\n"
            ));
            body.push_str(&alternative_part);
        } else {
            body.push_str(&format!(
                "Content-Type: multipart/related; boundary=\"{related}\"\r\n\r\n"
            ));
            body.push_str(&format!("--{related}\r\n"));
            body.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{alternative}\"\r\n\r\n"
            ));
            body.push_str(&alternative_part);
            for file in &inline {
                body.push_str(&format!("--{related}\r\n"));
                body.push_str(&inline_part(file));
            }
            body.push_str(&format!("--{related}--\r\n"));
        }
        for file in attached {
            body.push_str(&format!("--{mixed}\r\n"));
            body.push_str(&attachment_part(file));
        }
        body.push_str(&format!("--{mixed}--\r\n"));
    }
    let mut bytes = headers.into_bytes();
    bytes.extend(body.into_bytes());
    bytes
}

fn content_blobs<'a>(plain: &'a str, html: &'a str, files: &'a [ResolvedFile]) -> Vec<&'a [u8]> {
    let mut blobs = vec![plain.as_bytes(), html.as_bytes()];
    for file in files {
        blobs.push(file.bytes.as_slice());
    }
    blobs
}

fn fresh_boundary(tag: &str, blobs: &[&[u8]]) -> String {
    let mut salt = 0u32;
    loop {
        let token = format!("fell{tag}{:016x}{salt:08x}", next_token());
        let marker = format!("--{token}");
        if blobs
            .iter()
            .all(|blob| !contains_bytes(blob, marker.as_bytes()))
        {
            return token;
        }
        salt = salt.wrapping_add(1);
    }
}

fn fresh_message_id() -> String {
    format!("{:016x}.{:08x}@compose.fell", next_token(), next_token())
}

fn next_token() -> u64 {
    let n = TOKEN.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ n.rotate_left(17)
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|window| window == needle)
}

fn alternative_part(boundary: &str, plain: &str, html: &str) -> String {
    format!(
        "--{boundary}\r\n{plain_part}--{boundary}\r\n{html_part}--{boundary}--\r\n",
        plain_part = text_part("plain", plain),
        html_part = text_part("html", html),
    )
}

fn text_part(kind: &str, text: &str) -> String {
    format!(
        "Content-Type: text/{kind}; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n{body}",
        body = quoted_printable(text)
    )
}

fn inline_part(file: &ResolvedFile) -> String {
    let cid = super::markdown::content_id(&file.path);
    format!(
        "Content-Type: {mime}\r\nContent-Transfer-Encoding: base64\r\nContent-ID: <{cid}>\r\nContent-Disposition: inline; filename=\"{name}\"\r\n\r\n{data}\r\n",
        mime = mime_type(&file.path),
        name = header_token(&file.display_name),
        data = base64_wrapped(&file.bytes)
    )
}

fn attachment_part(file: &ResolvedFile) -> String {
    format!(
        "Content-Type: {mime}\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"{name}\"\r\n\r\n{data}\r\n",
        mime = mime_type(&file.path),
        name = header_token(&file.display_name),
        data = base64_wrapped(&file.bytes)
    )
}

pub fn attachment_digest(files: &[ResolvedFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut hash: u64 = 0xcbf29ce484222325;
    for file in files {
        for byte in file.path.as_bytes().iter().chain(file.bytes.iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
    }
    format!("{hash:016x}")
}

fn mime_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".txt") || lower.ends_with(".md") {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

fn header_line(name: &str, value: &str) -> String {
    let mut line = format!("{name}: ");
    let mut width = line.len();
    for (index, word) in fold_words(value).into_iter().enumerate() {
        let gap = usize::from(index > 0);
        if width + gap + word.len() > 78 && width > name.len() + 2 {
            line.push_str("\r\n ");
            width = 1;
        } else if index > 0 {
            line.push(' ');
            width += 1;
        }
        line.push_str(&word);
        width += word.len();
    }
    line.push_str("\r\n");
    line
}

fn fold_words(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

fn encode_addresses(addresses: &[String]) -> String {
    addresses
        .iter()
        .map(|address| encode_address(address))
        .collect::<Vec<_>>()
        .join(", ")
}

fn encode_address(raw: &str) -> String {
    let Some((name, email)) = mailbox_parts(raw) else {
        return encode_phrase(raw);
    };
    let email = strip_breaks(&email);
    match name {
        Some(name) => format!("{} <{email}>", encode_phrase(&name)),
        None => email,
    }
}

fn encode_phrase(name: &str) -> String {
    let name = strip_breaks(name);
    if name.chars().all(|ch| ch.is_ascii() && !ch.is_control()) {
        if name
            .chars()
            .any(|ch| matches!(ch, ',' | ';' | ':' | '"' | '\\' | '<' | '>' | '@'))
        {
            format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            name
        }
    } else {
        encoded_words(&name)
    }
}

fn encode_unstructured(value: &str) -> String {
    let value = strip_breaks(value);
    if value.chars().all(|ch| ch.is_ascii() && !ch.is_control()) {
        value
    } else {
        encoded_words(&value)
    }
}

fn encoded_words(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        let mut end = (index + 45).min(bytes.len());
        while end > index && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == index {
            end = (index + 1).min(bytes.len());
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str("=?utf-8?B?");
        out.push_str(&base64_raw(&bytes[index..end]));
        out.push_str("?=");
        index = end;
    }
    out
}

fn strip_breaks(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect()
}

fn angle_id(raw: &str) -> String {
    let id = strip_breaks(raw.trim());
    if id.starts_with('<') && id.ends_with('>') {
        id
    } else {
        format!("<{id}>")
    }
}

fn rfc5322_now() -> String {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    rfc5322_date(unix)
}

fn rfc5322_date(unix: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (year, month, day, hour, minute, second, weekday) = civil_from_unix(unix);
    format!(
        "{}, {day:02} {} {year:04} {hour:02}:{minute:02}:{second:02} +0000",
        WEEKDAYS[weekday],
        MONTHS[(month - 1) as usize]
    )
}

fn civil_from_unix(unix: i64) -> (i32, u32, u32, u32, u32, u32, usize) {
    let days = unix.div_euclid(86_400);
    let tod = unix.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    if month <= 2 {
        year += 1;
    }
    (
        year as i32,
        month,
        day,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
        days.rem_euclid(7) as usize,
    )
}

fn header_token(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\r' | '\n' => '_',
            _ => ch,
        })
        .collect()
}

fn base64_raw(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let n = (u32::from(data[index]) << 16)
            | (u32::from(data[index + 1]) << 8)
            | u32::from(data[index + 2]);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        index += 3;
    }
    let rest = data.len() - index;
    if rest == 1 {
        let n = u32::from(data[index]) << 16;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let n = (u32::from(data[index]) << 16) | (u32::from(data[index + 1]) << 8);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

fn base64_wrapped(data: &[u8]) -> String {
    let raw = base64_raw(data);
    let mut out = String::new();
    let mut column = 0;
    for ch in raw.chars() {
        if column == 76 {
            out.push_str("\r\n");
            column = 0;
        }
        out.push(ch);
        column += 1;
    }
    out
}

fn quoted_printable(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<&str> = normalized.split('\n').collect();
    if normalized.ends_with('\n') {
        lines.pop();
    }
    let mut out = String::new();
    for line in lines {
        let bytes = line.as_bytes();
        let mut current = String::new();
        for (index, byte) in bytes.iter().copied().enumerate() {
            let last = index + 1 == bytes.len();
            let token = if byte == b'='
                || !(32..127).contains(&byte)
                || ((byte == b' ' || byte == b'\t') && last)
            {
                format!("={byte:02X}")
            } else {
                (byte as char).to_string()
            };
            if current.len() + token.len() > 75 {
                out.push_str(&current);
                out.push_str("=\r\n");
                current.clear();
            }
            current.push_str(&token);
        }
        out.push_str(&current);
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccountId;
    use mail_parser::MessageParser;

    #[test]
    fn a_second_angle_address_is_not_encoded_as_the_mailbox() {
        for raw in [
            "Ada <ada@example.com> <evil@example.com>",
            "ada@example.com<evil@example.com>",
        ] {
            let encoded = encode_address(raw);
            assert!(
                encoded.starts_with('"') && encoded.ends_with('"'),
                "{raw} -> {encoded}"
            );
            assert!(
                !encoded.ends_with("<evil@example.com>"),
                "{raw} -> {encoded}"
            );
        }
        assert_eq!(
            encode_address("Ada <ada@example.com>"),
            "Ada <ada@example.com>"
        );
    }

    fn fields() -> DraftFields {
        DraftFields {
            from: "jon@example.com".into(),
            to: vec!["Ada <ada@example.com>".into()],
            cc: vec![],
            bcc: vec![],
            subject: "Re: Q3".into(),
            in_reply_to: Some("<abc@example.com>".into()),
            attachments: vec![],
        }
    }

    fn sample(body: &str, files: Vec<ResolvedFile>) -> Outgoing {
        build_outgoing(AccountId("a".into()), &fields(), body, &files)
    }

    fn parse(bytes: &[u8]) -> mail_parser::Message<'_> {
        MessageParser::new().parse(bytes).expect("mime parses")
    }

    fn assert_smtp_lines(mime: &[u8]) {
        for line in mime.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            assert!(line.len() <= 998, "line is {} octets", line.len());
        }
    }

    #[test]
    fn alternative_carries_markdown_and_html() {
        let outgoing = sample("# Title\n\n- item\n", Vec::new());
        let message = parse(&outgoing.mime);
        let text = message.body_text(0).unwrap();
        let html = message.body_html(0).unwrap();
        assert!(text.contains("# Title"));
        assert!(text.contains("- item"));
        assert!(html.contains("<h1"));
        assert!(html.contains("<li>"));
        assert_smtp_lines(&outgoing.mime);
    }

    #[test]
    fn inline_image_is_a_cid_part_and_remote_urls_are_absent() {
        let outgoing = sample(
            "![chart](chart.png)\n\n![x](https://evil.example/a.png)\n",
            vec![ResolvedFile {
                path: "chart.png".into(),
                display_name: "chart.png".into(),
                bytes: b"png".to_vec(),
                inline: true,
            }],
        );
        let mime = String::from_utf8(outgoing.mime.clone()).unwrap();
        assert!(mime.contains("multipart/related"));
        assert!(mime.contains("Content-ID: <chart.png>"));
        let injected = sample(
            "![x](a.png)\n",
            vec![ResolvedFile {
                path: "a.png\r\nBcc: evil@example.com".into(),
                display_name: "a.png".into(),
                bytes: b"png".to_vec(),
                inline: true,
            }],
        );
        let injected_mime = String::from_utf8(injected.mime).unwrap();
        assert!(!injected_mime.contains("\r\nBcc: evil@example.com"));
        assert!(injected_mime.contains("Content-ID: <a.pngBcc: evil@example.com>"));
        assert!(!outgoing.html.contains("evil.example"));
        assert!(outgoing.html.contains("cid:chart.png"));
        assert!(outgoing.plain.contains("![chart](chart.png)"));
        let message = parse(&outgoing.mime);
        assert!(
            message
                .parts()
                .any(|part| part.decoded().as_ref() == b"png")
        );
    }

    #[test]
    fn base64_wraps_every_payload_line() {
        let wrapped = base64_wrapped(&vec![0x61; 300]);
        for line in wrapped.split("\r\n") {
            assert!(line.len() <= 76, "{line}");
            assert!(!line.is_empty());
        }
    }

    #[test]
    fn epoch_date_is_thursday_first_of_january() {
        assert_eq!(rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
    }

    #[test]
    fn round_trip_covers_the_wire_format() {
        let image = vec![0x5a; 120_000];
        let mut draft = fields();
        draft.from = "Jón <jon@example.com>".into();
        draft.to = vec![
            "Ada Löve <ada@example.com>".into(),
            "Bob <bob@example.com>".into(),
        ];
        draft.cc = vec!["Chloé <chloe@example.com>".into()];
        draft.bcc = vec!["hidden@example.com".into()];
        draft.subject = "Ré: Q3".into();
        draft.in_reply_to = Some("<parent@example.com>".into());
        let body = format!("before --fell-alt-9f3a after\n{}\n", "x".repeat(2_500));
        let outgoing = build_outgoing(
            AccountId("a".into()),
            &draft,
            &body,
            &[ResolvedFile {
                path: "chart.png".into(),
                display_name: "chart.png".into(),
                bytes: image.clone(),
                inline: true,
            }],
        );
        assert_smtp_lines(&outgoing.mime);
        let raw = String::from_utf8(outgoing.mime.clone()).unwrap();
        assert!(!raw.to_ascii_lowercase().contains("\r\nbcc:"));
        assert!(raw.contains("<ada@example.com>"));
        assert!(raw.contains("<bob@example.com>"));
        assert!(raw.contains("<jon@example.com>"));
        assert_eq!(outgoing.draft.bcc, vec!["hidden@example.com".to_string()]);

        let message = parse(&outgoing.mime);
        assert!(message.bcc().is_none());
        assert!(message.date().is_some());
        assert!(message.message_id().is_some_and(|id| !id.is_empty()));
        let from = message.from().unwrap().first().unwrap();
        assert_eq!(from.name(), Some("Jón"));
        assert_eq!(from.address(), Some("jon@example.com"));
        let to = message.to().unwrap();
        let names = to.mailboxes().map(|item| item.name()).collect::<Vec<_>>();
        assert!(names.contains(&Some("Ada Löve")));
        assert!(names.contains(&Some("Bob")));
        let emails = to
            .mailboxes()
            .map(|item| item.address().unwrap_or(""))
            .collect::<Vec<_>>();
        assert!(emails.contains(&"ada@example.com"));
        assert!(emails.contains(&"bob@example.com"));
        let text = message.body_text(0).unwrap().to_string();
        assert!(text.contains("--fell-alt-9f3a"));
        assert!(text.contains(&"x".repeat(2_500)));
        let reply = message
            .in_reply_to()
            .unwrap()
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        assert!(reply.iter().any(|id| id.contains("parent@example.com")));
        let references = message
            .references()
            .unwrap()
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        assert!(
            references
                .iter()
                .any(|id| id.contains("parent@example.com"))
        );
        assert!(
            message
                .parts()
                .any(|part| part.decoded().as_ref() == image.as_slice())
        );
    }
}
