//! `draft.md` is YAML frontmatter plus the markdown body.
//!
//! Fell writes the keys in a fixed order. Parsing uses `saphyr` (MIT or
//! Apache-2.0). A YAML error never discards the body the editor already has.

use saphyr::{LoadableYamlNode, MarkedYamlOwned, YamlDataOwned};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    pub path: String,
    pub name: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DraftFields {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub in_reply_to: Option<String>,
    pub attachments: Vec<AttachmentRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftIssue {
    pub line: usize,
    pub message: String,
    pub blocks_send: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDraft {
    pub fields: DraftFields,
    pub body: String,
    pub issues: Vec<DraftIssue>,
}

#[derive(Debug)]
pub struct Unparseable {
    pub message: String,
}

/// Split a draft file. `Err` means the file is not a draft document; the caller
/// keeps the live buffer and rewrites the file.
pub fn parse_draft(text: &str) -> Result<ParsedDraft, Unparseable> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let Some((yaml, body, yaml_line_offset)) = split_frontmatter(&normalized) else {
        return Err(Unparseable {
            message: "draft.md has no YAML frontmatter".into(),
        });
    };
    parse_yaml(&yaml, body, yaml_line_offset)
}

fn parse_yaml(
    yaml: &str,
    body: String,
    yaml_line_offset: usize,
) -> Result<ParsedDraft, Unparseable> {
    let mut issues = Vec::new();
    let docs = match MarkedYamlOwned::load_from_str(yaml) {
        Ok(docs) => docs,
        Err(err) => {
            return Err(Unparseable {
                message: format!("draft.md frontmatter: {err}"),
            });
        }
    };
    if docs.len() != 1 {
        return Err(Unparseable {
            message: "draft.md frontmatter must be one YAML document".into(),
        });
    }
    let root = &docs[0];
    let Some(mapping) = root.data.as_mapping() else {
        return Err(Unparseable {
            message: "draft.md frontmatter must be a mapping".into(),
        });
    };
    let mut fields = DraftFields::default();
    let mut seen = Vec::new();
    for (key_node, value_node) in mapping.iter() {
        let Some(key) = key_node.data.as_str() else {
            issues.push(issue(
                file_line(key_node, yaml_line_offset),
                "frontmatter keys must be strings",
            ));
            continue;
        };
        seen.push(key.to_string());
        let line = file_line(value_node, yaml_line_offset);
        match key {
            "from" => match scalar_string(&value_node.data) {
                Ok(value) => fields.from = value,
                Err(message) => issues.push(issue(line, &message)),
            },
            "subject" => match scalar_string(&value_node.data) {
                Ok(value) => fields.subject = value,
                Err(message) => issues.push(issue(line, &message)),
            },
            "in_reply_to" => match optional_string(&value_node.data) {
                Ok(value) => fields.in_reply_to = value.filter(|item| !item.is_empty()),
                Err(message) => issues.push(issue(line, &message)),
            },
            "to" | "cc" | "bcc" => {
                if let Ok(list) = string_list(value_node, yaml_line_offset, &mut issues) {
                    match key {
                        "to" => fields.to = list,
                        "cc" => fields.cc = list,
                        _ => fields.bcc = list,
                    }
                }
            }
            "attachments" => {
                fields.attachments = attachment_list(value_node, yaml_line_offset, &mut issues);
            }
            _ => issues.push(DraftIssue {
                line: file_line(key_node, yaml_line_offset),
                message: format!("unknown frontmatter key `{key}`"),
                blocks_send: true,
            }),
        }
    }
    let _ = seen;
    check_addresses("to", &fields.to, yaml, yaml_line_offset, &mut issues);
    check_addresses("cc", &fields.cc, yaml, yaml_line_offset, &mut issues);
    check_addresses("bcc", &fields.bcc, yaml, yaml_line_offset, &mut issues);
    if !fields.from.is_empty() && !valid_address(&fields.from) {
        issues.push(issue(
            key_line(yaml, "from", yaml_line_offset),
            "from is not an email address",
        ));
    }
    Ok(ParsedDraft {
        fields,
        body,
        issues,
    })
}

pub fn write_draft(fields: &DraftFields, body: &str) -> String {
    let mut out = String::from("---\n");
    out.push_str("from: ");
    out.push_str(&yaml_plain_or_quoted(&fields.from));
    out.push('\n');
    out.push_str("to: ");
    out.push_str(&yaml_flow(&fields.to));
    out.push('\n');
    out.push_str("cc: ");
    out.push_str(&yaml_flow(&fields.cc));
    out.push('\n');
    out.push_str("bcc: ");
    out.push_str(&yaml_flow(&fields.bcc));
    out.push('\n');
    out.push_str("subject: ");
    out.push_str(&yaml_quote(&fields.subject));
    out.push('\n');
    out.push_str("in_reply_to: ");
    match &fields.in_reply_to {
        Some(value) => out.push_str(&yaml_quote(value)),
        None => out.push_str("\"\""),
    }
    out.push('\n');
    out.push_str("attachments:");
    if fields.attachments.is_empty() {
        out.push_str(" []\n");
    } else {
        out.push('\n');
        for attachment in &fields.attachments {
            out.push_str("  - path: ");
            out.push_str(&yaml_plain_or_quoted(&attachment.path));
            out.push('\n');
            if let Some(name) = &attachment.name {
                out.push_str("    name: ");
                out.push_str(&yaml_quote(name));
                out.push('\n');
            }
        }
    }
    out.push_str("---\n");
    out.push_str(body);
    if !body.is_empty() && !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn valid_address(raw: &str) -> bool {
    address_problem(raw).is_none() && email_of(raw).is_some()
}

pub fn email_of(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(start) = text.rfind('<') {
        let after = &text[start + 1..];
        let end = after.find('>')?;
        let email = after[..end].trim();
        if email.contains('@') && !email.contains(char::is_whitespace) {
            return Some(email.to_ascii_lowercase());
        }
        return None;
    }
    if text.contains('@') && !text.contains(char::is_whitespace) && !text.contains('<') {
        return Some(text.to_ascii_lowercase());
    }
    None
}

pub fn parse_address_line(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => {
                escaped = true;
                current.push(ch);
            }
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            ',' if !quoted => {
                let item = current.trim().to_string();
                if !item.is_empty() {
                    if !valid_address(&item) {
                        return Err(format!("invalid address `{item}`"));
                    }
                    out.push(item);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if quoted || escaped {
        return Err("unclosed quote in address".into());
    }
    let item = current.trim().to_string();
    if !item.is_empty() {
        if !valid_address(&item) {
            return Err(format!("invalid address `{item}`"));
        }
        out.push(item);
    }
    Ok(out)
}

pub fn format_address_line(addresses: &[String]) -> String {
    addresses.join(", ")
}

fn split_frontmatter(text: &str) -> Option<(String, String, usize)> {
    let mut rest = text;
    let first = if let Some(at) = rest.find('\n') {
        let line = &rest[..at];
        rest = &rest[at + 1..];
        line
    } else {
        let line = rest;
        rest = "";
        line
    };
    if first.trim() != "---" {
        return None;
    }
    let close = rest.find("\n---").or_else(|| {
        if rest.trim_end() == "---" || rest.starts_with("---") {
            Some(0)
        } else {
            None
        }
    })?;
    let yaml = rest[..close].to_string();
    let after_yaml = &rest[close..];
    let body = if let Some(stripped) = after_yaml.strip_prefix('\n') {
        let line_end = stripped.find('\n').unwrap_or(stripped.len());
        if stripped[..line_end].trim() != "---" {
            return None;
        }
        if line_end >= stripped.len() {
            String::new()
        } else {
            stripped[line_end + 1..].to_string()
        }
    } else if after_yaml.trim() == "---" {
        String::new()
    } else {
        return None;
    };
    Some((yaml, body, 1))
}

fn file_line(node: &MarkedYamlOwned, offset: usize) -> usize {
    node.span.start.line().saturating_add(offset)
}

fn issue(line: usize, message: &str) -> DraftIssue {
    DraftIssue {
        line,
        message: message.to_string(),
        blocks_send: true,
    }
}

fn scalar_string(data: &YamlDataOwned<MarkedYamlOwned>) -> Result<String, String> {
    if data.is_null() {
        return Ok(String::new());
    }
    if let Some(text) = data.as_str() {
        return Ok(text.to_string());
    }
    if let Some(number) = data.as_integer() {
        return Ok(number.to_string());
    }
    Err("expected a string".into())
}

fn optional_string(data: &YamlDataOwned<MarkedYamlOwned>) -> Result<Option<String>, String> {
    if data.is_null() {
        return Ok(None);
    }
    scalar_string(data).map(Some)
}

fn string_list(
    node: &MarkedYamlOwned,
    offset: usize,
    issues: &mut Vec<DraftIssue>,
) -> Result<Vec<String>, ()> {
    let Some(seq) = node.data.as_sequence() else {
        issues.push(issue(
            file_line(node, offset),
            "addresses must be a YAML list",
        ));
        return Err(());
    };
    let mut out = Vec::new();
    for item in seq {
        match scalar_string(&item.data) {
            Ok(value) => out.push(value),
            Err(message) => issues.push(issue(file_line(item, offset), &message)),
        }
    }
    Ok(out)
}

fn attachment_list(
    node: &MarkedYamlOwned,
    offset: usize,
    issues: &mut Vec<DraftIssue>,
) -> Vec<AttachmentRef> {
    let Some(seq) = node.data.as_sequence() else {
        issues.push(issue(
            file_line(node, offset),
            "attachments must be a YAML list",
        ));
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in seq {
        let line = file_line(item, offset);
        let Some(mapping) = item.data.as_mapping() else {
            issues.push(issue(line, "attachment must be a mapping"));
            continue;
        };
        let mut path = None;
        let mut name = None;
        for (key_node, value_node) in mapping.iter() {
            let Some(key) = key_node.data.as_str() else {
                issues.push(issue(
                    file_line(key_node, offset),
                    "attachment key must be a string",
                ));
                continue;
            };
            match key {
                "path" => match scalar_string(&value_node.data) {
                    Ok(value) => path = Some(value),
                    Err(message) => issues.push(issue(file_line(value_node, offset), &message)),
                },
                "name" => match scalar_string(&value_node.data) {
                    Ok(value) => name = Some(value),
                    Err(message) => issues.push(issue(file_line(value_node, offset), &message)),
                },
                other => issues.push(issue(
                    file_line(key_node, offset),
                    &format!("unknown attachment key `{other}`"),
                )),
            }
        }
        match path {
            Some(path) if !path.is_empty() => out.push(AttachmentRef { path, name, line }),
            _ => issues.push(issue(line, "attachment path is missing")),
        }
    }
    out
}

fn check_addresses(
    field: &str,
    addresses: &[String],
    yaml: &str,
    offset: usize,
    issues: &mut Vec<DraftIssue>,
) {
    for address in addresses {
        if let Some(problem) = address_problem(address) {
            issues.push(issue(
                key_line(yaml, field, offset),
                &format!("{field}: {problem}"),
            ));
        } else if email_of(address).is_none() {
            issues.push(issue(
                key_line(yaml, field, offset),
                &format!("{field}: `{address}` is not an email address"),
            ));
        }
    }
}

fn address_problem(raw: &str) -> Option<&'static str> {
    let text = raw.trim();
    if text.is_empty() || text.chars().any(|ch| ch.is_control()) {
        return Some("invalid address");
    }
    let mut quoted = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => return Some("put each address in its own list item"),
            _ => {}
        }
    }
    if quoted || escaped {
        return Some("unclosed quote");
    }
    None
}

fn key_line(yaml: &str, key: &str, offset: usize) -> usize {
    for (index, line) in yaml.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with(key) && trimmed[key.len()..].trim_start().starts_with(':') {
            return index + 1 + offset;
        }
    }
    offset + 1
}

fn yaml_quote(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn yaml_plain_or_quoted(text: &str) -> String {
    if text.is_empty() {
        return "\"\"".into();
    }
    let safe = text
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '@' | '.' | '+' | '_' | '-' | '/'));
    if safe {
        text.to_string()
    } else {
        yaml_quote(text)
    }
}

fn yaml_flow(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".into();
    }
    let mut out = String::from("[");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&yaml_quote(item));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_key_order_and_quoted_commas() {
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["\"Predmore, Ada\" <ada@example.com>".into()],
            cc: vec![],
            bcc: vec![],
            subject: "Re: Q3 numbers".into(),
            in_reply_to: Some("<abc@example.com>".into()),
            attachments: vec![AttachmentRef {
                path: "q3.xlsx".into(),
                name: Some("Q3 numbers.xlsx".into()),
                line: 0,
            }],
        };
        let text = write_draft(&fields, "Body in markdown.\n");
        assert!(text.find("from:").unwrap() < text.find("to:").unwrap());
        assert!(text.find("to:").unwrap() < text.find("subject:").unwrap());
        assert!(text.find("subject:").unwrap() < text.find("attachments:").unwrap());
        let parsed = parse_draft(&text).unwrap();
        assert_eq!(parsed.fields.to, fields.to);
        assert!(valid_address(&fields.to[0]));
        assert_eq!(email_of(&fields.to[0]).as_deref(), Some("ada@example.com"));
        assert_eq!(parsed.fields.subject, fields.subject);
        assert_eq!(
            parsed.fields.attachments[0].name.as_deref(),
            Some("Q3 numbers.xlsx")
        );
        assert_eq!(parsed.body, "Body in markdown.\n");
        assert!(parsed.issues.is_empty(), "{:?}", parsed.issues);
    }

    #[test]
    fn unknown_key_names_its_line_and_keeps_the_body() {
        let text = "---\nfrom: a@b.com\nto: []\ncc: []\nbcc: []\nsubject: \"hi\"\nin_reply_to: \"\"\nevil: true\nattachments: []\n---\nKeep me\n";
        let parsed = parse_draft(text).unwrap();
        assert_eq!(parsed.body, "Keep me\n");
        assert!(parsed.issues.iter().any(|issue| {
            issue.message.contains("evil") && issue.line > 1 && issue.blocks_send
        }));
    }

    #[test]
    fn bad_address_names_its_line() {
        let text = "---\nfrom: a@b.com\nto: [\"not-an-email\"]\ncc: []\nbcc: []\nsubject: \"hi\"\nin_reply_to: \"\"\nattachments: []\n---\nBody\n";
        let parsed = parse_draft(text).unwrap();
        assert!(
            parsed
                .issues
                .iter()
                .any(|issue| issue.message.contains("not-an-email"))
        );
        assert_eq!(parsed.body, "Body\n");
    }

    #[test]
    fn closing_delimiter_without_a_newline_is_an_empty_body() {
        let text = "---\nfrom: a@b.com\nto: []\ncc: []\nbcc: []\nsubject: \"hi\"\nin_reply_to: \"\"\nattachments: []\n---";
        let parsed = parse_draft(text).unwrap();
        assert_eq!(parsed.body, "");
        assert_eq!(parsed.fields.subject, "hi");
    }

    #[test]
    fn broken_yaml_is_unparseable() {
        let err = parse_draft("---\n[unterminated\n---\nbody\n").unwrap_err();
        assert!(!err.message.is_empty());
    }
}
