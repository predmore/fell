//! Plain markdown is the text the user edits. The same block model is what the
//! native HTML renderer paints, so a draft and a received message share a path.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledDoc {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Byte range in the markdown source. Absent for HTML input.
    pub source: Option<(usize, usize)>,
    pub body: BlockBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockBody {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph {
        inlines: Vec<Inline>,
    },
    List {
        ordered: bool,
        items: Vec<Vec<Inline>>,
    },
    Quote {
        blocks: Vec<Block>,
    },
    Code {
        text: String,
    },
    Image {
        alt: String,
        src: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Em(Vec<Inline>),
    Strong(Vec<Inline>),
    Code(String),
    Link { text: String, href: String },
    Image { alt: String, src: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    Plain,
    Marker,
    Strong,
    Em,
    Code,
    LinkText,
    LinkUrl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleSpan {
    pub start: usize,
    pub end: usize,
    pub kind: StyleKind,
}

struct SrcLine<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

pub fn markdown_to_doc(source: &str) -> StyledDoc {
    let lines = split_lines(source);
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];
        if line.text.trim().is_empty() {
            index += 1;
            continue;
        }
        if line.text.starts_with("```") {
            let (block, next) = take_fence(&lines, index);
            blocks.push(block);
            index = next;
            continue;
        }
        if is_heading(line.text) {
            let level = heading_level(line.text);
            let content = line.text[level as usize..].trim_start();
            let marker = line.text.len() - content.len();
            blocks.push(Block {
                source: Some((line.start, line.end)),
                body: BlockBody::Heading {
                    level,
                    inlines: parse_inlines(&line.text[marker..]),
                },
            });
            index += 1;
            continue;
        }
        if line.text.starts_with('>') {
            let (block, next) = take_quote(&lines, index);
            blocks.push(block);
            index = next;
            continue;
        }
        if list_marker(line.text).is_some() {
            let (block, next) = take_list(&lines, index);
            blocks.push(block);
            index = next;
            continue;
        }
        let (text, end, next) = take_paragraph(&lines, index);
        let inlines = parse_inlines(text.trim());
        if let [Inline::Image { alt, src }] = inlines.as_slice() {
            blocks.push(Block {
                source: Some((lines[index].start, end)),
                body: BlockBody::Image {
                    alt: alt.clone(),
                    src: src.clone(),
                },
            });
        } else {
            blocks.push(Block {
                source: Some((lines[index].start, end)),
                body: BlockBody::Paragraph { inlines },
            });
        }
        index = next;
    }
    StyledDoc { blocks }
}

pub fn html_to_doc(html: &str) -> StyledDoc {
    let cleaned = strip_block(html, "script");
    let cleaned = strip_block(&cleaned, "style");
    let mut blocks = Vec::new();
    let mut paragraph: Vec<Inline> = Vec::new();
    let bytes = cleaned.as_bytes();
    let mut index = 0;
    let mut list: Option<(bool, Vec<Vec<Inline>>)> = None;
    let mut quote: Option<Vec<Block>> = None;
    let flush_paragraph = |paragraph: &mut Vec<Inline>, blocks: &mut Vec<Block>| {
        if paragraph.is_empty() {
            return;
        }
        let inlines = std::mem::take(paragraph);
        blocks.push(Block {
            source: None,
            body: BlockBody::Paragraph { inlines },
        });
    };
    while index < bytes.len() {
        if bytes[index] != b'<' {
            let start = index;
            while index < bytes.len() && bytes[index] != b'<' {
                index += 1;
            }
            let text = decode_entities(&cleaned[start..index]);
            if text.chars().any(|ch| !ch.is_whitespace()) || !paragraph.is_empty() {
                push_text(&mut paragraph, &text);
            }
            continue;
        }
        let close = cleaned[index..].find('>').map(|at| index + at);
        let Some(close) = close else {
            push_text(&mut paragraph, &cleaned[index..]);
            break;
        };
        let tag = &cleaned[index + 1..close];
        index = close + 1;
        let (name, closing) = tag_name(tag);
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "br" => push_text(&mut paragraph, "\n"),
            "p" | "div" if closing => flush_paragraph(&mut paragraph, &mut blocks),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if !closing => {
                flush_paragraph(&mut paragraph, &mut blocks);
                let level = name.as_bytes()[1] - b'0';
                let (inner, next) = take_until(&cleaned, index, &name);
                blocks.push(Block {
                    source: None,
                    body: BlockBody::Heading {
                        level,
                        inlines: vec![Inline::Text(decode_entities(inner))],
                    },
                });
                index = next;
            }
            "ul" | "ol" if !closing => {
                flush_paragraph(&mut paragraph, &mut blocks);
                list = Some((name == "ol", Vec::new()));
            }
            "ul" | "ol" if closing => {
                if let Some((ordered, items)) = list.take() {
                    blocks.push(Block {
                        source: None,
                        body: BlockBody::List { ordered, items },
                    });
                }
            }
            "li" if !closing => {
                let (inner, next) = take_until(&cleaned, index, "li");
                if let Some((_, items)) = list.as_mut() {
                    items.push(vec![Inline::Text(decode_entities(inner.trim()))]);
                } else {
                    push_text(&mut paragraph, &decode_entities(inner.trim()));
                }
                index = next;
            }
            "blockquote" if !closing => {
                flush_paragraph(&mut paragraph, &mut blocks);
                quote = Some(Vec::new());
            }
            "blockquote" if closing => {
                if let Some(inner) = quote.take() {
                    blocks.push(Block {
                        source: None,
                        body: BlockBody::Quote { blocks: inner },
                    });
                }
            }
            "pre" if !closing => {
                flush_paragraph(&mut paragraph, &mut blocks);
                let (inner, next) = take_until(&cleaned, index, "pre");
                let text = decode_entities(&strip_tags(inner));
                blocks.push(Block {
                    source: None,
                    body: BlockBody::Code { text },
                });
                index = next;
            }
            "a" if !closing => {
                let href = attr(tag, "href").unwrap_or_default();
                let (inner, next) = take_until(&cleaned, index, "a");
                paragraph.push(Inline::Link {
                    text: decode_entities(&strip_tags(inner)),
                    href,
                });
                index = next;
            }
            "strong" | "b" if !closing => {
                let (inner, next) = take_until(&cleaned, index, &name);
                paragraph.push(Inline::Strong(vec![Inline::Text(decode_entities(
                    &strip_tags(inner),
                ))]));
                index = next;
            }
            "em" | "i" if !closing => {
                let (inner, next) = take_until(&cleaned, index, &name);
                paragraph.push(Inline::Em(vec![Inline::Text(decode_entities(
                    &strip_tags(inner),
                ))]));
                index = next;
            }
            "code" if !closing => {
                let (inner, next) = take_until(&cleaned, index, "code");
                paragraph.push(Inline::Code(decode_entities(inner)));
                index = next;
            }
            "img" => {
                let alt = attr(tag, "alt").unwrap_or_default();
                let src = attr(tag, "src").unwrap_or_default();
                if paragraph.is_empty() || quote.is_some() || list.is_some() {
                    blocks.push(Block {
                        source: None,
                        body: BlockBody::Image { alt, src },
                    });
                } else {
                    paragraph.push(Inline::Image { alt, src });
                }
            }
            _ => {}
        }
        if let Some(bucket) = quote.as_mut()
            && !paragraph.is_empty()
            && matches!(name.as_str(), "p" | "div" | "br")
        {
            let inlines = std::mem::take(&mut paragraph);
            bucket.push(Block {
                source: None,
                body: BlockBody::Paragraph { inlines },
            });
        }
    }
    flush_paragraph(&mut paragraph, &mut blocks);
    if let Some(inner) = quote.take() {
        blocks.push(Block {
            source: None,
            body: BlockBody::Quote { blocks: inner },
        });
    }
    StyledDoc { blocks }
}

pub fn doc_to_html(doc: &StyledDoc) -> String {
    let mut body = String::new();
    for block in &doc.blocks {
        body.push_str(&block_html(block));
    }
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>\
body{{font-family:Georgia,'Iowan Old Style',serif;font-size:16px;line-height:1.45;color:#222}}\
h1,h2,h3,h4{{font-family:-apple-system,'Segoe UI',sans-serif;line-height:1.2}}\
a{{color:#1a56db}}blockquote{{margin:0.6em 0;padding-left:0.8em;border-left:3px solid #ccc;color:#444}}\
code,pre{{font-family:ui-monospace,Consolas,monospace}}pre{{background:#f6f6f6;padding:0.6em}}\
img{{max-width:100%}}\
</style></head><body>{body}</body></html>"
    )
}

/// Styles for the cursor line. Markers stay in the string so editing is plain text.
pub fn line_styles(line: &str) -> Vec<StyleSpan> {
    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let mut index = 0;
    if let Some(level) = heading_marker_len(line) {
        push_span(&mut spans, 0, level, StyleKind::Marker);
        index = level;
    } else if let Some(len) = quote_marker_len(line) {
        push_span(&mut spans, 0, len, StyleKind::Marker);
        index = len;
    } else if let Some(len) = list_marker(line) {
        push_span(&mut spans, 0, len, StyleKind::Marker);
        index = len;
    }
    while index < bytes.len() {
        if bytes[index] == b'`'
            && let Some(end) = line[index + 1..].find('`')
        {
            let close = index + 1 + end;
            push_span(&mut spans, index, index + 1, StyleKind::Marker);
            push_span(&mut spans, index + 1, close, StyleKind::Code);
            push_span(&mut spans, close, close + 1, StyleKind::Marker);
            index = close + 1;
            continue;
        }
        if line[index..].starts_with("**") || line[index..].starts_with("__") {
            let marker = &line[index..index + 2];
            if let Some(end) = line[index + 2..].find(marker) {
                let close = index + 2 + end;
                push_span(&mut spans, index, index + 2, StyleKind::Marker);
                push_span(&mut spans, index + 2, close, StyleKind::Strong);
                push_span(&mut spans, close, close + 2, StyleKind::Marker);
                index = close + 2;
                continue;
            }
        }
        if bytes[index] == b'*' || bytes[index] == b'_' {
            let marker = &line[index..index + 1];
            if let Some(end) = line[index + 1..].find(marker) {
                let close = index + 1 + end;
                if close > index + 1 {
                    push_span(&mut spans, index, index + 1, StyleKind::Marker);
                    push_span(&mut spans, index + 1, close, StyleKind::Em);
                    push_span(&mut spans, close, close + 1, StyleKind::Marker);
                    index = close + 1;
                    continue;
                }
            }
        }
        if bytes[index] == b'!'
            && line[index..].starts_with("![")
            && let Some((alt, _src, end)) = image_at(&line[index..])
        {
            let abs = index;
            push_span(&mut spans, abs, abs + 2, StyleKind::Marker);
            push_span(
                &mut spans,
                abs + 2,
                abs + 2 + alt.len(),
                StyleKind::LinkText,
            );
            let url_at = abs + 2 + alt.len();
            push_span(&mut spans, url_at, end + abs, StyleKind::LinkUrl);
            index = abs + end;
            continue;
        }
        if bytes[index] == b'['
            && let Some((text, _href, end)) = link_at(&line[index..])
        {
            push_span(&mut spans, index, index + 1, StyleKind::Marker);
            push_span(
                &mut spans,
                index + 1,
                index + 1 + text.len(),
                StyleKind::LinkText,
            );
            let url_at = index + 1 + text.len();
            push_span(&mut spans, url_at, index + end, StyleKind::LinkUrl);
            index += end;
            continue;
        }
        let start = index;
        index += utf8_len(bytes[index]);
        while index < bytes.len() {
            let ch = bytes[index];
            if ch == b'`' || ch == b'*' || ch == b'_' || ch == b'[' || ch == b'!' {
                break;
            }
            index += utf8_len(ch);
        }
        push_span(&mut spans, start, index, StyleKind::Plain);
    }
    if spans.is_empty() {
        spans.push(StyleSpan {
            start: 0,
            end: line.len(),
            kind: StyleKind::Plain,
        });
    }
    spans
}

fn push_span(spans: &mut Vec<StyleSpan>, start: usize, end: usize, kind: StyleKind) {
    if end > start {
        spans.push(StyleSpan { start, end, kind });
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first < 0xE0 {
        2
    } else if first < 0xF0 {
        3
    } else {
        4
    }
}

fn split_lines(source: &str) -> Vec<SrcLine<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (offset, _) in source.match_indices('\n') {
        lines.push(SrcLine {
            text: &source[start..offset],
            start,
            end: offset + 1,
        });
        start = offset + 1;
    }
    if start < source.len() || source.is_empty() {
        lines.push(SrcLine {
            text: &source[start..],
            start,
            end: source.len(),
        });
    }
    lines
}

fn take_fence(lines: &[SrcLine<'_>], start: usize) -> (Block, usize) {
    let mut text = String::new();
    let mut index = start + 1;
    while index < lines.len() && !lines[index].text.starts_with("```") {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(lines[index].text);
        index += 1;
    }
    let end = if index < lines.len() {
        let close = index;
        index += 1;
        lines[close].end
    } else {
        lines[start].end
    };
    (
        Block {
            source: Some((lines[start].start, end)),
            body: BlockBody::Code { text },
        },
        index,
    )
}

fn take_quote(lines: &[SrcLine<'_>], start: usize) -> (Block, usize) {
    let mut inner = String::new();
    let mut index = start;
    while index < lines.len() && lines[index].text.starts_with('>') {
        if !inner.is_empty() {
            inner.push('\n');
        }
        let rest = lines[index].text.trim_start_matches('>').trim_start();
        inner.push_str(rest);
        index += 1;
    }
    let doc = markdown_to_doc(&inner);
    (
        Block {
            source: Some((lines[start].start, lines[index - 1].end)),
            body: BlockBody::Quote { blocks: doc.blocks },
        },
        index,
    )
}

fn take_list(lines: &[SrcLine<'_>], start: usize) -> (Block, usize) {
    let ordered = lines[start]
        .text
        .trim_start()
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_digit());
    let mut items = Vec::new();
    let mut index = start;
    while index < lines.len() {
        let Some(marker) = list_marker(lines[index].text) else {
            break;
        };
        let item_ordered = lines[index]
            .text
            .trim_start()
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit());
        if item_ordered != ordered {
            break;
        }
        items.push(parse_inlines(lines[index].text[marker..].trim()));
        index += 1;
    }
    (
        Block {
            source: Some((lines[start].start, lines[index - 1].end)),
            body: BlockBody::List { ordered, items },
        },
        index,
    )
}

fn take_paragraph(lines: &[SrcLine<'_>], start: usize) -> (String, usize, usize) {
    let mut text = lines[start].text.to_string();
    let mut index = start + 1;
    while index < lines.len() {
        let line = lines[index].text;
        if line.trim().is_empty()
            || line.starts_with("```")
            || line.starts_with('>')
            || is_heading(line)
            || list_marker(line).is_some()
        {
            break;
        }
        text.push(' ');
        text.push_str(line.trim());
        index += 1;
    }
    let end = lines[index - 1].end;
    (text, end, index)
}

fn parse_inlines(text: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Inline>| {
        if !buf.is_empty() {
            out.push(Inline::Text(std::mem::take(buf)));
        }
    };
    while index < bytes.len() {
        if bytes[index] == b'`'
            && let Some(end) = text[index + 1..].find('`')
        {
            flush(&mut buf, &mut out);
            let close = index + 1 + end;
            out.push(Inline::Code(text[index + 1..close].to_string()));
            index = close + 1;
            continue;
        }
        if text[index..].starts_with("**") || text[index..].starts_with("__") {
            let marker = &text[index..index + 2];
            if let Some(end) = text[index + 2..].find(marker) {
                flush(&mut buf, &mut out);
                let close = index + 2 + end;
                out.push(Inline::Strong(parse_inlines(&text[index + 2..close])));
                index = close + 2;
                continue;
            }
        }
        if (bytes[index] == b'*' || bytes[index] == b'_')
            && let Some(end) = text[index + 1..].find(if bytes[index] == b'*' { '*' } else { '_' })
        {
            let close = index + 1 + end;
            if close > index + 1 {
                flush(&mut buf, &mut out);
                out.push(Inline::Em(parse_inlines(&text[index + 1..close])));
                index = close + 1;
                continue;
            }
        }
        if bytes[index] == b'!'
            && text[index..].starts_with("![")
            && let Some((alt, src, end)) = image_at(&text[index..])
        {
            flush(&mut buf, &mut out);
            out.push(Inline::Image {
                alt: alt.to_string(),
                src: src.to_string(),
            });
            index += end;
            continue;
        }
        if bytes[index] == b'['
            && let Some((label, href, end)) = link_at(&text[index..])
        {
            flush(&mut buf, &mut out);
            out.push(Inline::Link {
                text: label.to_string(),
                href: href.to_string(),
            });
            index += end;
            continue;
        }
        buf.push(text[index..].chars().next().unwrap_or('\0'));
        index += utf8_len(bytes[index]);
    }
    flush(&mut buf, &mut out);
    if out.is_empty() {
        out.push(Inline::Text(String::new()));
    }
    out
}

fn image_at(text: &str) -> Option<(&str, &str, usize)> {
    if !text.starts_with("![") {
        return None;
    }
    let alt_end = text[2..].find("](")?;
    let src_start = 2 + alt_end + 2;
    let src_end = text[src_start..].find(')')?;
    let end = src_start + src_end + 1;
    Some((
        &text[2..2 + alt_end],
        &text[src_start..src_start + src_end],
        end,
    ))
}

fn link_at(text: &str) -> Option<(&str, &str, usize)> {
    if !text.starts_with('[') {
        return None;
    }
    let label_end = text[1..].find("](")?;
    let href_start = 1 + label_end + 2;
    let href_end = text[href_start..].find(')')?;
    let end = href_start + href_end + 1;
    Some((
        &text[1..1 + label_end],
        &text[href_start..href_start + href_end],
        end,
    ))
}

fn is_heading(line: &str) -> bool {
    heading_marker_len(line).is_some() && heading_level(line) > 0
}

fn heading_level(line: &str) -> u8 {
    line.bytes().take_while(|byte| *byte == b'#').count() as u8
}

fn heading_marker_len(line: &str) -> Option<usize> {
    let level = heading_level(line);
    if (1..=6).contains(&level) {
        let rest = &line[level as usize..];
        if rest.starts_with(' ') || rest.starts_with('\t') {
            let spaces = rest
                .bytes()
                .take_while(|byte| *byte == b' ' || *byte == b'\t')
                .count();
            return Some(level as usize + spaces);
        }
    }
    None
}

fn quote_marker_len(line: &str) -> Option<usize> {
    if !line.starts_with('>') {
        return None;
    }
    let rest = line[1..].bytes().take_while(|byte| *byte == b' ').count();
    Some(1 + rest)
}

fn list_marker(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let bytes = trimmed.as_bytes();
    if bytes
        .first()
        .is_some_and(|byte| *byte == b'-' || *byte == b'*')
        && bytes
            .get(1)
            .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
    {
        let spaces = bytes[1..]
            .iter()
            .take_while(|byte| **byte == b' ' || **byte == b'\t')
            .count();
        return Some(indent + 1 + spaces);
    }
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits > 0 && bytes.get(digits) == Some(&b'.') {
        let after = digits + 1;
        if bytes
            .get(after)
            .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
        {
            let spaces = bytes[after..]
                .iter()
                .take_while(|byte| **byte == b' ' || **byte == b'\t')
                .count();
            return Some(indent + after + spaces);
        }
    }
    None
}

fn block_html(block: &Block) -> String {
    match &block.body {
        BlockBody::Heading { level, inlines } => {
            let tag = match level {
                1 => "h1",
                2 => "h2",
                3 => "h3",
                4 => "h4",
                5 => "h5",
                _ => "h6",
            };
            format!(
                "<{tag} style=\"font-family:-apple-system,'Segoe UI',sans-serif;line-height:1.2\">{inner}</{tag}>",
                inner = inlines_html(inlines)
            )
        }
        BlockBody::Paragraph { inlines } => {
            format!(
                "<p style=\"margin:0.4em 0\">{inner}</p>",
                inner = inlines_html(inlines)
            )
        }
        BlockBody::List { ordered, items } => {
            let tag = if *ordered { "ol" } else { "ul" };
            let mut html = format!("<{tag} style=\"margin:0.4em 0;padding-left:1.2em\">");
            for item in items {
                html.push_str("<li>");
                html.push_str(&inlines_html(item));
                html.push_str("</li>");
            }
            html.push_str("</");
            html.push_str(tag);
            html.push('>');
            html
        }
        BlockBody::Quote { blocks } => {
            let mut html = String::from(
                "<blockquote style=\"margin:0.6em 0;padding-left:0.8em;border-left:3px solid #ccc;color:#444\">",
            );
            for block in blocks {
                html.push_str(&block_html(block));
            }
            html.push_str("</blockquote>");
            html
        }
        BlockBody::Code { text } => format!(
            "<pre style=\"font-family:ui-monospace,Consolas,monospace;background:#f6f6f6;padding:0.6em\"><code>{}</code></pre>",
            escape(text)
        ),
        BlockBody::Image { alt, src } => image_html(alt, src),
    }
}

fn inlines_html(inlines: &[Inline]) -> String {
    let mut html = String::new();
    for inline in inlines {
        html.push_str(&match inline {
            Inline::Text(text) => escape(text),
            Inline::Em(inner) => format!("<em>{}</em>", inlines_html(inner)),
            Inline::Strong(inner) => format!("<strong>{}</strong>", inlines_html(inner)),
            Inline::Code(text) => format!(
                "<code style=\"font-family:ui-monospace,Consolas,monospace\">{}</code>",
                escape(text)
            ),
            Inline::Link { text, href } => {
                if is_remote_url(href) {
                    format!(
                        "<a href=\"{}\" style=\"color:#1a56db\">{}</a>",
                        escape(href),
                        escape(text)
                    )
                } else {
                    escape(text)
                }
            }
            Inline::Image { alt, src } => image_html(alt, src),
        });
    }
    html
}

fn image_html(alt: &str, src: &str) -> String {
    if is_remote_url(src) || src.starts_with("data:") {
        return format!(
            "<span style=\"color:#666\">{} (remote image blocked)</span>",
            escape(alt)
        );
    }
    let cid = content_id(src);
    format!(
        "<img src=\"cid:{cid}\" alt=\"{alt}\" style=\"max-width:100%\">",
        cid = escape(&cid),
        alt = escape(alt)
    )
}

pub(crate) fn content_id(src: &str) -> String {
    src.trim_start_matches("./")
        .chars()
        .filter(|ch| !matches!(ch, '\r' | '\n' | '<' | '>'))
        .collect()
}

fn is_remote_url(src: &str) -> bool {
    let lower = src.trim().to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.contains("://")
}

pub(super) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn strip_block(html: &str, tag: &str) -> String {
    let mut out = String::new();
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut index = 0;
    while index < html.len() {
        if let Some(at) = lower[index..].find(&open) {
            out.push_str(&html[index..index + at]);
            let from = index + at;
            if let Some(end) = lower[from..].find(&close) {
                let after = from + end + close.len();
                let rest = &html[after..];
                let skip = rest.find('>').map(|pos| pos + 1).unwrap_or(0);
                index = after + skip;
            } else {
                // No closing tag: drop only the opening tag, so a stray
                // `<style` cannot blank everything after it.
                let rest = &html[from..];
                index = from + rest.find('>').map(|pos| pos + 1).unwrap_or(rest.len());
            }
        } else {
            out.push_str(&html[index..]);
            break;
        }
    }
    out
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut index = 0;
    let bytes = html.as_bytes();
    while index < bytes.len() {
        if bytes[index] == b'<'
            && let Some(end) = html[index..].find('>')
        {
            index += end + 1;
            continue;
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

fn take_until<'a>(html: &'a str, from: usize, name: &str) -> (&'a str, usize) {
    let close = format!("</{name}");
    let lower = html[from..].to_ascii_lowercase();
    if let Some(at) = lower.find(&close) {
        let end = from + at;
        let after = end + close.len();
        let skip = html[after..].find('>').map(|pos| pos + 1).unwrap_or(0);
        (&html[from..end], after + skip)
    } else {
        (&html[from..], html.len())
    }
}

fn tag_name(tag: &str) -> (&str, bool) {
    let tag = tag.trim();
    let closing = tag.starts_with('/');
    let name = tag.trim_start_matches('/').trim();
    let end = name
        .find(|ch: char| ch.is_whitespace() || ch == '/')
        .unwrap_or(name.len());
    (&name[..end], closing || tag.ends_with('/'))
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&needle)?;
    let rest = tag[at + needle.len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &rest[1..];
        let end = inner.find(quote)?;
        Some(decode_entities(&inner[..end]))
    } else {
        let end = rest
            .find(|ch: char| ch.is_whitespace() || ch == '>')
            .unwrap_or(rest.len());
        Some(decode_entities(&rest[..end]))
    }
}

fn push_text(inlines: &mut Vec<Inline>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Inline::Text(existing)) = inlines.last_mut() {
        existing.push_str(text);
    } else {
        inlines.push(Inline::Text(text.to_string()));
    }
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_and_html_share_a_document() {
        let markdown = "# Hi\n\n- one\n- two\n\n> quoted\n\nSee [notes](https://example.com).\n";
        let from_markdown = markdown_to_doc(markdown);
        let html = doc_to_html(&from_markdown);
        assert!(html.contains("<h1"));
        assert!(html.contains("<li>"));
        assert!(html.contains("https://example.com"));
        assert!(html.contains("<blockquote"));
        let from_html = html_to_doc(&html);
        assert!(
            from_html
                .blocks
                .iter()
                .any(|block| matches!(block.body, BlockBody::Heading { level: 1, .. }))
        );
        assert!(
            from_html
                .blocks
                .iter()
                .any(|block| matches!(block.body, BlockBody::List { .. }))
        );
    }

    #[test]
    fn remote_images_are_not_links() {
        let doc = markdown_to_doc("![x](https://evil.example/a.png)\n");
        let html = doc_to_html(&doc);
        assert!(!html.contains("evil.example"));
        assert!(html.contains("remote image blocked"));
    }

    #[test]
    fn cursor_line_keeps_markers() {
        let spans = line_styles("**bold** and [a](https://example.com)");
        let covered: String = spans
            .iter()
            .map(|span| "x".repeat(span.end - span.start))
            .collect();
        assert_eq!(covered.len(), "**bold** and [a](https://example.com)".len());
        assert!(spans.iter().any(|span| span.kind == StyleKind::Marker));
        assert!(spans.iter().any(|span| span.kind == StyleKind::Strong));
    }

    #[test]
    fn html_drops_script() {
        let doc = html_to_doc("<p>Hi</p><script>alert(1)</script><p>There</p>");
        let flat = format!("{doc:?}");
        assert!(!flat.contains("alert"));
        assert!(flat.contains("Hi"));
    }

    #[test]
    fn an_unclosed_style_tag_keeps_the_text_after_it() {
        let kept = strip_block("<style>p{color:red}<p>Keep the margin.</p>", "style");
        assert!(kept.contains("Keep the margin."), "{kept}");
        assert!(!kept.contains("<style"), "{kept}");
        assert_eq!(strip_block("a<script>x</script>b", "script"), "ab");
    }
}
