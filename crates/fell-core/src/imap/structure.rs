//! The MIME tree reported by UID FETCH BODYSTRUCTURE, without message bytes.

use crate::{Disposition, MimePart, PartRole, TransferEncoding};

use super::rfc822::{CidReferences, clean_name, section};

#[derive(Debug, Default)]
pub(crate) struct BodyStructure {
    pub html: bool,
    pub parts: Vec<MimePart>,
}

enum Value {
    List(Vec<Value>),
    Text(String),
    Nil,
}

impl Value {
    fn list(&self) -> Option<&[Self]> {
        match self {
            Self::List(values) => Some(values),
            _ => None,
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }
}

/// FETCH framing already bounded the reply and separated out its literals.
/// Bound the nested lists too, both for parsing and when they are dropped.
fn value<'a>(
    input: &mut &str,
    literals: &mut impl Iterator<Item = Option<&'a [u8]>>,
    depth: usize,
) -> Option<Value> {
    if depth > 64 {
        return None;
    }
    *input = input.trim_start_matches(' ');
    match input.as_bytes().first()? {
        b'(' => {
            *input = &input[1..];
            let mut items = Vec::new();
            loop {
                *input = input.trim_start_matches(' ');
                if let Some(rest) = input.strip_prefix(')') {
                    *input = rest;
                    return Some(Value::List(items));
                }
                items.push(value(input, literals, depth + 1)?);
            }
        }
        b'"' => {
            let mut chars = input[1..].char_indices();
            let mut text = String::new();
            while let Some((at, ch)) = chars.next() {
                match ch {
                    '"' => {
                        *input = &input[at + 2..];
                        return Some(Value::Text(text));
                    }
                    '\\' => text.push(chars.next()?.1),
                    _ => text.push(ch),
                }
            }
            None
        }
        b'{' => {
            let (_, rest) = input.split_once('}')?;
            *input = rest;
            Some(Value::Text(
                String::from_utf8_lossy(literals.next()??).into_owned(),
            ))
        }
        _ => {
            let end = input.find([' ', '(', ')']).unwrap_or(input.len());
            if end == 0 {
                return None;
            }
            let (atom, rest) = input.split_at(end);
            *input = rest;
            Some(if atom.eq_ignore_ascii_case("NIL") {
                Value::Nil
            } else {
                Value::Text(atom.to_owned())
            })
        }
    }
}

pub(super) fn parse<'a>(
    mut input: &str,
    mut literals: impl Iterator<Item = Option<&'a [u8]>>,
) -> Option<BodyStructure> {
    let root = value(&mut input, &mut literals, 0)?;
    let mut structure = BodyStructure::default();
    walk(root.list()?, "", 0, true, &mut structure)?;
    Some(structure)
}

fn filename(parameters: Option<&Value>, name: &str) -> Option<String> {
    let (pairs, _) = parameters?.list()?.as_chunks::<2>();
    let mut header = String::from("Content-Disposition: attachment");
    for [key, raw] in pairs {
        let key = key.text()?.to_ascii_lowercase();
        let Some(suffix) = key.strip_prefix(name) else {
            continue;
        };
        if !suffix.is_empty()
            && !(suffix.starts_with('*')
                && suffix.bytes().all(|ch| ch == b'*' || ch.is_ascii_digit()))
        {
            continue;
        }
        let Some(raw) = raw.text() else { continue };
        header.push_str("; filename");
        header.push_str(suffix);
        header.push_str("=\"");
        for ch in raw.chars().filter(|ch| !ch.is_control()) {
            if matches!(ch, '\\' | '"') {
                header.push('\\');
            }
            header.push(ch);
        }
        header.push('"');
    }
    header.push_str("\r\n\r\n");
    let parsed = mail_parser::MessageParser::default().parse(header.as_bytes())?;
    parsed.attachment_name().and_then(clean_name)
}

fn walk(
    items: &[Value],
    prefix: &str,
    nested: usize,
    body: bool,
    out: &mut BodyStructure,
) -> Option<()> {
    if items.first()?.list().is_some() {
        let children = items.iter().map_while(Value::list).collect::<Vec<_>>();
        let subtype = items.get(children.len())?.text()?;
        for (index, child) in children.into_iter().enumerate() {
            // Only the root of multipart/related can be its text body.
            let body = body && (!subtype.eq_ignore_ascii_case("related") || index == 0);
            walk(child, &section(prefix, index + 1), nested, body, out)?;
        }
        return Some(());
    }

    let kind = items.first()?.text()?.to_ascii_lowercase();
    let subtype = items.get(1)?.text()?.to_ascii_lowercase();
    let mime_type = format!("{kind}/{subtype}");
    let token = |ch: char| ch.is_ascii_alphanumeric() || "!#$&^_.+-/".contains(ch);
    let mime_type = if mime_type.len() <= 127 && mime_type.chars().all(token) {
        mime_type
    } else {
        "application/octet-stream".into()
    };
    let is_message = mime_type == "message/rfc822";
    let extension = if is_message {
        11
    } else if kind == "text" {
        9
    } else {
        8
    };
    let disposition_fields = items.get(extension).and_then(Value::list);
    let disposition_kind = disposition_fields
        .and_then(|values| values.first()?.text())
        .unwrap_or("")
        .to_ascii_lowercase();
    let disposition = match disposition_kind.as_str() {
        "attachment" => Disposition::Attachment,
        "inline" => Disposition::Inline,
        _ => Disposition::Unspecified,
    };
    let filename = disposition_fields
        .and_then(|values| filename(values.get(1), "filename"))
        .or_else(|| filename(items.get(2), "name"));
    if body
        && matches!(mime_type.as_str(), "text/html" | "text/plain")
        && disposition != Disposition::Attachment
        && filename.is_none()
    {
        out.html |= mime_type == "text/html";
        return Some(());
    }
    if out.parts.len() >= 256 {
        return Some(());
    }
    let encoded_size = items.get(6)?.text()?.parse::<u32>().ok()? as u64;
    let encoding = match items.get(5)?.text()?.to_ascii_lowercase().as_str() {
        "base64" => TransferEncoding::Base64,
        "quoted-printable" => TransferEncoding::QuotedPrintable,
        _ => TransferEncoding::Identity,
    };
    let content_id = items
        .get(3)
        .and_then(Value::text)
        .and_then(|id| clean_name(id.trim_matches(['<', '>'])));
    let number = if prefix.is_empty() {
        "1".to_owned()
    } else {
        prefix.to_owned()
    };
    let mut part = MimePart {
        section: number.clone(),
        role: PartRole::Attachment,
        mime_type,
        filename,
        // BODYSTRUCTURE reports the wire size. The final list replaces it
        // with the decoded size once the message source has been parsed.
        size: encoded_size,
        encoded_size,
        encoding,
        content_id,
        disposition,
        nested: nested > 0,
    };
    part.size = part.estimated_decoded_size();
    part.role = CidReferences::new(None).role_for(&part);
    out.parts.push(part);
    if is_message && nested < 4 {
        let inner = items.get(8)?.list()?;
        let prefix = if inner.first()?.list().is_some() {
            number
        } else {
            section(&number, 1)
        };
        walk(inner, &prefix, nested + 1, false, out)?;
    }
    Some(())
}
