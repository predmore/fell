use std::ops::Deref;

use scraper::Html;
use scraper::node::Node;

use crate::document::{Block, ImageSource, Inline, ListItem, StyledDocument};

/// HTML only with a real signal: a doctype, or a known tag.
///
/// `<ada@example.com>` is an address, not a tag. The store does not keep a
/// content type yet; when it does, `text/html` is the other signal.
pub fn looks_like_html(body: &str) -> bool {
    let lower = body.trim_start().to_ascii_lowercase();
    if lower.starts_with("<!doctype") {
        return true;
    }
    let Some(rest) = lower.strip_prefix('<') else {
        return false;
    };
    let name_len = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .count();
    if name_len == 0 {
        return false;
    }
    let name = &rest[..name_len];
    let boundary = rest[name_len..].chars().next();
    let closed = matches!(boundary, Some(ch) if ch.is_whitespace() || ch == '>' || ch == '/');
    closed && known_html_tag(name)
}

fn known_html_tag(name: &str) -> bool {
    matches!(
        name,
        "a" | "abbr"
            | "address"
            | "article"
            | "aside"
            | "b"
            | "blockquote"
            | "body"
            | "br"
            | "caption"
            | "center"
            | "code"
            | "col"
            | "colgroup"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "em"
            | "figcaption"
            | "figure"
            | "font"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hr"
            | "html"
            | "i"
            | "img"
            | "li"
            | "main"
            | "nav"
            | "noscript"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "span"
            | "strong"
            | "style"
            | "svg"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "u"
            | "ul"
    )
}

pub fn html_to_document(html: &str) -> StyledDocument {
    let dom = Html::parse_document(html);
    let blocks = children_blocks(dom.root_element());
    StyledDocument {
        blocks: blocks
            .into_iter()
            .filter(|block| !block_is_empty(block))
            .collect(),
    }
}

/// Plain text stored in `body_text` becomes paragraphs. HTML is parsed.
pub fn document_from_body(body: &str) -> StyledDocument {
    if looks_like_html(body) {
        html_to_document(body)
    } else {
        plain_to_document(body)
    }
}

pub fn plain_to_document(body: &str) -> StyledDocument {
    let mut blocks = Vec::new();
    for para in body.split("\n\n") {
        let text = para.trim();
        if text.is_empty() {
            continue;
        }
        let mut inlines = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if index > 0 {
                inlines.push(Inline::LineBreak);
            }
            inlines.push(Inline::Text(line.to_string()));
        }
        blocks.push(Block::Paragraph { inlines });
    }
    StyledDocument { blocks }
}

fn children_blocks(node: scraper::ElementRef<'_>) -> Vec<Block> {
    node.children().flat_map(node_blocks).collect()
}

fn node_blocks(node: ego_tree::NodeRef<'_, Node>) -> Vec<Block> {
    match node.value() {
        Node::Text(text) => {
            let text = normalize_ws(text.deref());
            if text.trim().is_empty() {
                Vec::new()
            } else {
                vec![Block::Paragraph {
                    inlines: vec![Inline::Text(text.trim().to_string())],
                }]
            }
        }
        Node::Element(element) => element_blocks(node, element.name()),
        _ => Vec::new(),
    }
}

fn element_blocks(node: ego_tree::NodeRef<'_, Node>, name: &str) -> Vec<Block> {
    match name {
        "script" | "style" | "head" | "meta" | "link" | "title" | "iframe" | "object" | "embed"
        | "input" | "button" | "textarea" => Vec::new(),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = name.as_bytes()[1].saturating_sub(b'0');
            vec![Block::Heading {
                level,
                inlines: collect_inlines(node),
            }]
        }
        "p" => paragraph(collect_inlines(node)),
        "blockquote" => vec![Block::Quote {
            blocks: children_of(node),
        }],
        "pre" => vec![Block::Code {
            text: raw_text(node).trim_matches('\n').to_string(),
        }],
        "ul" => vec![list_block(node, false)],
        "ol" => vec![list_block(node, true)],
        "hr" => vec![Block::ThematicBreak],
        "img" => node
            .value()
            .as_element()
            .map(image_block)
            .into_iter()
            .collect(),
        "table" => table_blocks(node),
        "br" => Vec::new(),
        _ => {
            if has_block_child(node) {
                children_of(node)
            } else {
                paragraph(collect_inlines(node))
            }
        }
    }
}

fn paragraph(inlines: Vec<Inline>) -> Vec<Block> {
    if inlines.is_empty() {
        Vec::new()
    } else {
        vec![Block::Paragraph { inlines }]
    }
}

fn children_of(node: ego_tree::NodeRef<'_, Node>) -> Vec<Block> {
    node.children().flat_map(node_blocks).collect()
}

fn list_block(node: ego_tree::NodeRef<'_, Node>, ordered: bool) -> Block {
    let mut items = Vec::new();
    for child in node.children() {
        let Some(element) = child.value().as_element() else {
            continue;
        };
        if element.name() != "li" {
            continue;
        }
        let blocks = if has_block_child(child) {
            children_of(child)
        } else {
            paragraph(collect_inlines(child))
        };
        if !blocks.is_empty() {
            items.push(ListItem { blocks });
        }
    }
    Block::List { ordered, items }
}

fn table_blocks(node: ego_tree::NodeRef<'_, Node>) -> Vec<Block> {
    let presentation = node.value().as_element().is_some_and(|element| {
        element.attr("role").is_some_and(|role| {
            role.eq_ignore_ascii_case("presentation") || role.eq_ignore_ascii_case("none")
        })
    });
    if presentation || table_count(node) > 1 {
        return children_of(node);
    }
    let mut header = Vec::new();
    let mut rows = Vec::new();
    collect_rows(node, true, &mut header, &mut rows);
    if header.is_empty() && rows.is_empty() {
        return children_of(node);
    }
    vec![Block::Table { header, rows }]
}

fn collect_rows(
    node: ego_tree::NodeRef<'_, Node>,
    root: bool,
    header: &mut Vec<Vec<Block>>,
    rows: &mut Vec<Vec<Vec<Block>>>,
) {
    for child in node.children() {
        let Some(element) = child.value().as_element() else {
            continue;
        };
        match element.name() {
            "table" if !root => continue,
            "tr" => {
                let cells = row_cells(child);
                if cells.is_empty() {
                    continue;
                }
                let all_header = child.children().all(|cell| {
                    cell.value()
                        .as_element()
                        .is_none_or(|element| element.name() != "td")
                }) && child.children().any(|cell| {
                    cell.value()
                        .as_element()
                        .is_some_and(|element| element.name() == "th")
                });
                if all_header && header.is_empty() {
                    header.extend(cells);
                } else {
                    rows.push(cells);
                }
            }
            _ => collect_rows(child, false, header, rows),
        }
    }
}

fn row_cells(row: ego_tree::NodeRef<'_, Node>) -> Vec<Vec<Block>> {
    row.children()
        .filter_map(|cell| {
            let name = cell.value().as_element()?.name();
            (name == "td" || name == "th").then(|| cell_blocks(cell))
        })
        .collect()
}

/// A cell keeps its paragraphs, lists, and quotes. Inlines are only for a cell
/// that has no block children, where a `<br>` is still a line break.
fn cell_blocks(cell: ego_tree::NodeRef<'_, Node>) -> Vec<Block> {
    let blocks = if has_block_child(cell) {
        children_of(cell)
    } else {
        paragraph(collect_inlines(cell))
    };
    blocks
        .into_iter()
        .filter(|block| !block_is_empty(block))
        .collect()
}

fn table_count(node: ego_tree::NodeRef<'_, Node>) -> usize {
    let here = node
        .value()
        .as_element()
        .is_some_and(|element| element.name() == "table") as usize;
    here + node.children().map(table_count).sum::<usize>()
}

fn has_block_child(node: ego_tree::NodeRef<'_, Node>) -> bool {
    node.children().any(|child| {
        child
            .value()
            .as_element()
            .is_some_and(|element| is_block_tag(element.name()))
            || has_block_child(child)
    })
}

fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "ul"
            | "tr"
            | "center"
    )
}

fn collect_inlines(node: ego_tree::NodeRef<'_, Node>) -> Vec<Inline> {
    let mut out = Vec::new();
    for child in node.children() {
        push_inline(child, &mut out);
    }
    trim_inlines(&mut out);
    out
}

fn push_inline(node: ego_tree::NodeRef<'_, Node>, out: &mut Vec<Inline>) {
    match node.value() {
        Node::Text(text) => {
            let text = normalize_ws(text.deref());
            if !text.is_empty() {
                out.push(Inline::Text(text));
            }
        }
        Node::Element(element) => match element.name() {
            "script" | "style" | "input" | "button" | "textarea" => {}
            "br" => out.push(Inline::LineBreak),
            "a" => out.push(Inline::Link {
                href: safe_href(element.attr("href").unwrap_or("")),
                children: collect_inlines(node),
            }),
            "em" | "i" => out.push(Inline::Emphasis(collect_inlines(node))),
            "strong" | "b" => out.push(Inline::Strong(collect_inlines(node))),
            "code" => out.push(Inline::Code(raw_text(node).trim().to_string())),
            "img" => out.push(image_inline(element)),
            _ => {
                for child in node.children() {
                    push_inline(child, out);
                }
            }
        },
        _ => {}
    }
}

fn trim_inlines(inlines: &mut Vec<Inline>) {
    if let Some(Inline::Text(text)) = inlines.first_mut() {
        *text = text.trim_start().to_string();
        if text.is_empty() {
            inlines.remove(0);
        }
    }
    if let Some(Inline::Text(text)) = inlines.last_mut() {
        *text = text.trim_end().to_string();
        if text.is_empty() {
            inlines.pop();
        }
    }
}

fn image_block(element: &scraper::node::Element) -> Block {
    let inline = image_inline(element);
    match inline {
        Inline::Image { source, alt } => Block::Image { source, alt },
        _ => Block::Paragraph {
            inlines: vec![inline],
        },
    }
}

fn image_inline(element: &scraper::node::Element) -> Inline {
    let alt = element.attr("alt").unwrap_or("").trim().to_string();
    let source = element
        .attr("src")
        .map(image_source)
        .unwrap_or(ImageSource::Remote(String::new()));
    Inline::Image { source, alt }
}

pub(crate) fn image_source_from_markdown(src: &str) -> ImageSource {
    image_source(src)
}

fn image_source(src: &str) -> ImageSource {
    let src = src.trim();
    if let Some(rest) = src.strip_prefix("cid:") {
        return ImageSource::Cid(rest.to_string());
    }
    if let Some(rest) = src.strip_prefix("data:") {
        return data_image(rest);
    }
    ImageSource::Remote(src.to_string())
}

fn data_image(rest: &str) -> ImageSource {
    let Some((meta, data)) = rest.split_once(',') else {
        return ImageSource::Remote(format!("data:{rest}"));
    };
    let mut parts = meta.split(';');
    let media_type = parts
        .next()
        .unwrap_or("application/octet-stream")
        .to_string();
    let base64 = meta.to_ascii_lowercase().contains("base64");
    if !base64 {
        return ImageSource::Data {
            media_type,
            bytes: data.as_bytes().to_vec(),
        };
    }
    // MIME wraps base64 at 76 columns, and a data URI may also carry a
    // percent-encoded newline. The standard engine rejects that whitespace.
    let payload = strip_data_payload(data);
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        payload.as_bytes(),
    )
    .or_else(|_| {
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD_NO_PAD,
            payload.as_bytes(),
        )
    });
    match decoded {
        Ok(bytes) => ImageSource::Data { media_type, bytes },
        Err(_) => ImageSource::Remote(format!("data:{rest}")),
    }
}

fn strip_data_payload(data: &str) -> String {
    let bytes = data.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && is_percent_encoded_space(&bytes[index + 1..index + 3])
        {
            index += 3;
            continue;
        }
        if !bytes[index].is_ascii_whitespace() {
            out.push(bytes[index] as char);
        }
        index += 1;
    }
    out
}

fn is_percent_encoded_space(bytes: &[u8]) -> bool {
    let hex = [bytes[0].to_ascii_lowercase(), bytes[1].to_ascii_lowercase()];
    matches!(
        hex,
        [b'0', b'a'] | [b'0', b'd'] | [b'0', b'9'] | [b'2', b'0'] | [b'0', b'c']
    )
}

fn safe_href(href: &str) -> String {
    let trimmed = href.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("data:text/html") {
        String::new()
    } else {
        trimmed.to_string()
    }
}

fn raw_text(node: ego_tree::NodeRef<'_, Node>) -> String {
    let mut out = String::new();
    for descendant in node.descendants() {
        if let Node::Text(text) = descendant.value() {
            out.push_str(text.deref());
        }
    }
    out
}

fn normalize_ws(text: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    if text.chars().next().is_some_and(|ch| ch.is_whitespace()) {
        out.push(' ');
    }
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        pending_space = false;
        out.push(ch);
    }
    if pending_space && !out.is_empty() && !out.ends_with(' ') {
        out.push(' ');
    }
    if out.trim().is_empty() {
        String::new()
    } else {
        out
    }
}

fn block_is_empty(block: &Block) -> bool {
    match block {
        Block::Heading { inlines, .. } | Block::Paragraph { inlines } => inlines.is_empty(),
        Block::List { items, .. } => items.is_empty(),
        Block::Quote { blocks } => blocks.is_empty(),
        Block::Code { text } => text.trim().is_empty(),
        Block::Table { header, rows } => header.is_empty() && rows.is_empty(),
        Block::Image { .. } | Block::ThematicBreak => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_keeps_emphasis_link_quote_list_and_image() {
        let doc = html_to_document(include_str!("../fixtures/native-personal-reply.html"));
        let text = doc.plain_text();
        assert!(text.contains("Thursday"));
        assert!(text.contains("bridge"));
        assert!(
            doc.blocks
                .iter()
                .any(|block| matches!(block, Block::Quote { .. }))
        );
        assert!(
            doc.blocks
                .iter()
                .any(|block| matches!(block, Block::List { .. }))
        );
        assert!(text.contains("cabin"));
        assert!(doc.plain_text().contains("sketch"));
    }

    #[test]
    fn simple_table_stays_a_table() {
        let doc = html_to_document(include_str!("../fixtures/native-personal-table.html"));
        let Block::Table { header, rows } = doc
            .blocks
            .iter()
            .find(|block| matches!(block, Block::Table { .. }))
            .expect("table")
        else {
            unreachable!("find");
        };
        assert_eq!(header.len(), 2);
        assert_eq!(rows.len(), 2);
        assert!(cell_text(&rows[0][1]).contains("Coffee"));
        assert!(!cell_text(&rows[1][1]).contains("Walkthrough"));

        let rich = html_to_document(
            "<table>\
                <tr><th>When</th><th>What</th></tr>\
                <tr>\
                    <td><p>Hello</p><p>World</p></td>\
                    <td><ul><li>One</li><li>Two</li></ul><ol><li>Third</li></ol></td>\
                    <td><blockquote><p>Said</p></blockquote></td>\
                </tr>\
            </table>",
        );
        let Block::Table { rows, .. } = rich
            .blocks
            .iter()
            .find(|block| matches!(block, Block::Table { .. }))
            .expect("rich table")
        else {
            unreachable!("find");
        };
        let paragraphs = &rows[0][0];
        assert_eq!(paragraphs.len(), 2, "{paragraphs:?}");
        let text = cell_text(paragraphs);
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World"), "{text}");
        assert!(!text.contains("HelloWorld"), "{text}");
        assert!(
            matches!(
                &rows[0][1][0],
                Block::List {
                    ordered: false,
                    items
                } if items.len() == 2
            ),
            "unordered marker: {:?}",
            rows[0][1]
        );
        assert!(
            matches!(&rows[0][1][1], Block::List { ordered: true, .. }),
            "ordered marker: {:?}",
            rows[0][1]
        );
        assert!(
            matches!(&rows[0][2][0], Block::Quote { .. }),
            "quote marker: {:?}",
            rows[0][2]
        );
        assert!(cell_text(&rows[0][2]).contains("Said"));
    }

    fn cell_text(blocks: &[Block]) -> String {
        StyledDocument {
            blocks: blocks.to_vec(),
        }
        .plain_text()
    }

    #[test]
    fn wrapped_data_uri_decodes() {
        let raw: Vec<u8> = (0..180).map(|index| (index % 251) as u8).collect();
        let payload = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &raw);
        assert!(payload.len() > 76);
        let mut wrapped = String::new();
        for (index, ch) in payload.chars().enumerate() {
            if index > 0 && index % 76 == 0 {
                wrapped.push('\n');
            }
            wrapped.push(ch);
        }
        let source = image_source(&format!("data:image/png;base64,{wrapped}"));
        match source {
            ImageSource::Data { bytes, .. } => assert_eq!(bytes, raw),
            other => panic!("wrapped payload became {other:?}"),
        }
        let break_at = payload.len() / 2;
        let mut encoded = String::new();
        for (index, ch) in payload.chars().enumerate() {
            if index == break_at {
                encoded.push_str("%0A");
            }
            encoded.push(ch);
        }
        let source = image_source(&format!("data:image/png;base64,{encoded}"));
        match source {
            ImageSource::Data { bytes, .. } => assert_eq!(bytes, raw),
            other => panic!("percent-encoded newline became {other:?}"),
        }
    }

    #[test]
    fn angle_address_stays_plain_text() {
        let body = "<ada@example.com> wrote:\n\nHello";
        assert!(!looks_like_html(body));
        let text = document_from_body(body).plain_text();
        assert!(text.contains("<ada@example.com>"));
        assert!(text.contains("Hello"));
    }

    #[test]
    fn form_and_noscript_keep_their_text() {
        let doc = html_to_document(
            "<form><p>See you Thursday</p><input type=\"text\"><button>Go</button><textarea>secret</textarea></form><noscript><p>Plain fallback</p></noscript><svg><text>Label</text></svg>",
        );
        let text = doc.plain_text();
        assert!(text.contains("See you Thursday"));
        assert!(text.contains("Plain fallback"));
        assert!(text.contains("Label"));
        assert!(!text.contains("Go"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn plain_text_is_paragraphs() {
        let doc = document_from_body("Hello\n\nNext");
        assert_eq!(doc.blocks.len(), 2);
        assert!(!looks_like_html("Hello"));
    }
}
