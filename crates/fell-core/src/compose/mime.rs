//! Multipart message built from the markdown the editor is showing.
//!
//! `text/plain` is that markdown. `text/html` comes from the shared renderer.
//! Inline images are CID parts. Remote URLs are not fetched.

use crate::send::Draft;

use super::frontmatter::DraftFields;
use super::markdown::{doc_to_html, markdown_to_doc};

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
    let mime = render_mime(fields, &plain, &html, files);
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
        mime,
        plain,
        html,
    }
}

fn render_mime(fields: &DraftFields, plain: &str, html: &str, files: &[ResolvedFile]) -> Vec<u8> {
    let mixed = "fell-mix-9f3a";
    let related = "fell-rel-9f3a";
    let alternative = "fell-alt-9f3a";
    let inline: Vec<_> = files.iter().filter(|file| file.inline).collect();
    let attached: Vec<_> = files.iter().filter(|file| !file.inline).collect();
    let mut headers = String::new();
    headers.push_str(&format!("From: {}\r\n", header_value(&fields.from)));
    headers.push_str(&format!("To: {}\r\n", header_value(&fields.to.join(", "))));
    if !fields.cc.is_empty() {
        headers.push_str(&format!("Cc: {}\r\n", header_value(&fields.cc.join(", "))));
    }
    if !fields.bcc.is_empty() {
        headers.push_str(&format!(
            "Bcc: {}\r\n",
            header_value(&fields.bcc.join(", "))
        ));
    }
    headers.push_str(&format!("Subject: {}\r\n", header_value(&fields.subject)));
    if let Some(reply) = &fields.in_reply_to {
        headers.push_str(&format!("In-Reply-To: {}\r\n", header_value(reply)));
    }
    headers.push_str("MIME-Version: 1.0\r\n");
    let mut body = String::new();
    let alternative_part = alternative_part(alternative, plain, html);
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

fn alternative_part(boundary: &str, plain: &str, html: &str) -> String {
    format!(
        "--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{plain}\r\n--{boundary}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{html}\r\n--{boundary}--\r\n"
    )
}

fn inline_part(file: &ResolvedFile) -> String {
    let cid = super::markdown::content_id(&file.path);
    format!(
        "Content-Type: {mime}\r\nContent-Transfer-Encoding: base64\r\nContent-ID: <{cid}>\r\nContent-Disposition: inline; filename=\"{name}\"\r\n\r\n{data}\r\n",
        mime = mime_type(&file.path),
        name = header_token(&file.display_name),
        data = base64(&file.bytes)
    )
}

fn attachment_part(file: &ResolvedFile) -> String {
    format!(
        "Content-Type: {mime}\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"{name}\"\r\n\r\n{data}\r\n",
        mime = mime_type(&file.path),
        name = header_token(&file.display_name),
        data = base64(&file.bytes)
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

fn header_value(value: &str) -> String {
    let clean: String = value
        .chars()
        .filter(|ch| *ch != '\r' && *ch != '\n')
        .collect();
    if clean.chars().all(|ch| ch.is_ascii() && !ch.is_control()) {
        clean
    } else {
        format!("=?utf-8?B?{}?=", base64(clean.as_bytes()))
    }
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

fn base64(data: &[u8]) -> String {
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
        if out.len().is_multiple_of(76) {
            out.push_str("\r\n");
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccountId;

    fn sample(body: &str, files: Vec<ResolvedFile>) -> Outgoing {
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["Ada <ada@example.com>".into()],
            cc: vec![],
            bcc: vec![],
            subject: "Re: Q3".into(),
            in_reply_to: Some("<abc@example.com>".into()),
            attachments: vec![],
        };
        build_outgoing(AccountId("a".into()), &fields, body, &files)
    }

    #[test]
    fn alternative_carries_markdown_and_html() {
        let outgoing = sample("# Title\n\n- item\n", Vec::new());
        let mime = String::from_utf8(outgoing.mime.clone()).unwrap();
        assert!(mime.contains("multipart/alternative"));
        assert!(mime.contains("# Title"));
        assert!(mime.contains("<h1"));
        assert!(mime.contains("<li>"));
        assert!(!mime.contains("multipart/related"));
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
        let mime = String::from_utf8(outgoing.mime).unwrap();
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
    }
}
