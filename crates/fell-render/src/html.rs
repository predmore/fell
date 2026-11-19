use std::ops::Deref;

use scraper::Html;
use scraper::node::Node;

use crate::document::{Align, Block, ImageSource, Inline, ListItem, StyledDocument};

/// How deep the native walk and the web serializer will recurse.
///
/// Past this, the rest of the subtree is kept as text. A few thousand nested
/// elements must not overflow the stack.
pub(crate) const MAX_HTML_DEPTH: u32 = 256;

/// HTML only with a real signal: a doctype, a comment, or a known tag that
/// actually closes or carries an attribute.
///
/// `<ada@example.com>` is an address. `<p Hello there>` is a sentence. The
/// store does not keep a content type yet; when it does, `text/html` is the
/// other signal.
pub fn looks_like_html(body: &str) -> bool {
    let rest = skip_html_preamble(body);
    if rest.starts_with("<!--") || rest.to_ascii_lowercase().starts_with("<!doctype") {
        return true;
    }
    leading_tag_is_markup(rest)
}

/// Drop a leading BOM and finished comments. An unclosed `<!--`, including
/// `<!--[if mso]>`, stays in place so the caller can treat it as markup.
fn skip_html_preamble(body: &str) -> &str {
    let mut rest = body;
    loop {
        rest = rest.trim_start();
        if let Some(stripped) = rest.strip_prefix('\u{feff}') {
            rest = stripped;
            continue;
        }
        let Some(after) = rest.strip_prefix("<!--") else {
            return rest;
        };
        // `<!--[if mso]>` is markup. The matching `-->` sits on `<![endif]-->`,
        // so skipping it would hide the HTML that follows the condition.
        if after.starts_with("[if") {
            return rest;
        }
        let Some(end) = after.find("-->") else {
            return rest;
        };
        rest = &after[end + 3..];
    }
}

fn leading_tag_is_markup(body: &str) -> bool {
    let Some(after_lt) = body.strip_prefix('<') else {
        return false;
    };
    let name_len = after_lt
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric())
        .count();
    if name_len == 0 {
        return false;
    }
    let name = &after_lt[..name_len];
    if !known_html_tag(&name.to_ascii_lowercase()) {
        return false;
    }
    tag_closes_or_has_attribute(&after_lt[name_len..])
}

/// `<p>` and `<br />` close. `<p class="x">` assigns an attribute.
/// `<p Hello there>` does neither: the `>` is not the next token.
fn tag_closes_or_has_attribute(after_name: &str) -> bool {
    let mut bytes = after_name.bytes();
    let mut skipped = 0;
    for byte in bytes.by_ref() {
        if !byte.is_ascii_whitespace() {
            break;
        }
        skipped += 1;
    }
    let rest = &after_name[skipped..];
    if rest.starts_with('>') || rest.starts_with("/>") {
        return true;
    }
    match rest.find('>') {
        Some(end) => rest[..end].contains('='),
        None => rest.contains('='),
    }
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

/// Mail is parsed with scripting off, so `<noscript>` children are elements.
/// The default parser treats that subtree as one text node of escaped markup.
pub(crate) fn parse_html(html: &str) -> Html {
    use html5ever::ParseOpts;
    use html5ever::parse_document;
    use tendril::TendrilSink;

    let mut opts = ParseOpts::default();
    opts.tree_builder.scripting_enabled = false;
    parse_document(scraper::HtmlTreeSink::new(Html::new_document()), opts).one(html)
}

pub fn html_to_document(html: &str) -> StyledDocument {
    let blocks = split_blank_lines(native_blocks(html, 0));
    let mut blocks: Vec<Block> = quote_marked_lines(blocks)
        .into_iter()
        .filter(|block| !block_is_empty(block))
        .collect();
    // A message set entirely in a small face is the sender's normal text,
    // not fine print.
    if !blocks.is_empty()
        && blocks
            .iter()
            .all(|block| matches!(block, Block::Small { .. }))
    {
        blocks = blocks
            .into_iter()
            .flat_map(|block| match block {
                Block::Small { blocks } => blocks,
                other => vec![other],
            })
            .collect();
    }
    StyledDocument { blocks }
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

fn native_blocks(html: &str, depth: u32) -> Vec<Block> {
    let mut dom = parse_html(html);
    prune_hidden(&mut dom);
    dom.tree
        .root()
        .children()
        .find(|node| node.value().is_element())
        .map_or_else(Vec::new, |root| children_of(root, depth))
}

fn node_blocks(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Block> {
    if depth >= MAX_HTML_DEPTH {
        return text_blocks(node);
    }
    match node.value() {
        Node::Text(text) => {
            let text = normalize_ws(text.deref());
            let text = text.trim_matches(is_ascii_html_ws);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![Block::Paragraph {
                    inlines: vec![Inline::Text(text.to_string())],
                }]
            }
        }
        Node::Element(element) => element_blocks(node, element.name(), depth),
        _ => Vec::new(),
    }
}

fn text_blocks(node: ego_tree::NodeRef<'_, Node>) -> Vec<Block> {
    let text = normalize_ws(&raw_text(node));
    let text = text.trim_matches(is_ascii_html_ws);
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Block::Paragraph {
            inlines: vec![Inline::Text(text.to_string())],
        }]
    }
}

fn element_blocks(node: ego_tree::NodeRef<'_, Node>, name: &str, depth: u32) -> Vec<Block> {
    let Some(element) = node.value().as_element() else {
        return Vec::new();
    };
    let blocks = visible_element_blocks(node, name, depth);
    styled_blocks(node, element, blocks)
}

/// What an element's own inline style says about what it holds. Fine
/// print is [`Block::Small`]. A short line set at 20 px or more is a
/// heading (`h2`), at 17 to 19 px a subheading (`h3`), as ESPs set their
/// headlines in a `<td>`. A line in a monospace face is code, and a bold
/// one is strong.
fn styled_blocks(
    node: ego_tree::NodeRef<'_, Node>,
    element: &scraper::node::Element,
    blocks: Vec<Block>,
) -> Vec<Block> {
    if blocks.is_empty() {
        return blocks;
    }
    if is_fine_print(element) || fine_print_inside(node) {
        return vec![Block::Small {
            blocks: blocks
                .into_iter()
                .flat_map(|block| match block {
                    Block::Small { blocks } => blocks,
                    other => vec![other],
                })
                .collect(),
        }];
    }
    let [Block::Paragraph { inlines }] = blocks.as_slice() else {
        return blocks;
    };
    if is_mono(element) {
        // A code set large (a booking or sign-in code) is one the reader
        // reads off and types: it keeps a little of that size.
        let large = text_size_px(node, 0).is_some_and(|size| size >= 20.0);
        return vec![Block::Code {
            text: inlines_plain(inlines),
            large,
        }];
    }
    if let Some(level) = heading_level(node)
        && reads_as_heading(inlines)
    {
        return vec![Block::Heading {
            level,
            inlines: inlines.clone(),
        }];
    }
    if is_bold(element) && !matches!(inlines.as_slice(), [Inline::Strong(_)]) {
        return vec![Block::Paragraph {
            inlines: vec![Inline::Strong(inlines.clone())],
        }];
    }
    blocks
}

/// The heading level an inline `font-size` implies, from the element or
/// the one element it wraps (`<td><span style="font-size:28px">`).
fn heading_level(node: ego_tree::NodeRef<'_, Node>) -> Option<u8> {
    let size = text_size_px(node, 0)?;
    if size >= 20.0 {
        Some(2)
    } else if size >= 17.0 {
        Some(3)
    } else {
        None
    }
}

fn text_size_px(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> Option<f32> {
    let element = node.value().as_element()?;
    if let Some(size) = font_px(element) {
        return Some(size);
    }
    if depth >= 4 {
        return None;
    }
    let mut only = None;
    for child in node.children() {
        match child.value() {
            Node::Text(text) if is_blank_text(text) => {}
            Node::Element(_) if only.is_none() => only = Some(child),
            _ => return None,
        }
    }
    text_size_px(only?, depth + 1)
}

/// A headline is short and one or two lines.
fn reads_as_heading(inlines: &[Inline]) -> bool {
    let text = inlines_plain(inlines);
    let breaks = inlines
        .iter()
        .filter(|inline| matches!(inline, Inline::LineBreak))
        .count();
    breaks <= 1 && text.chars().count() <= 100 && !text.trim().is_empty()
}

fn inlines_plain(inlines: &[Inline]) -> String {
    StyledDocument {
        blocks: vec![Block::Paragraph {
            inlines: inlines.to_vec(),
        }],
    }
    .plain_text()
}

fn visible_element_blocks(node: ego_tree::NodeRef<'_, Node>, name: &str, depth: u32) -> Vec<Block> {
    match name {
        "script" | "style" | "head" | "meta" | "link" | "title" | "embed" | "input" | "button"
        | "textarea" => Vec::new(),
        // The HTML parser stores iframe fallback as raw text, not elements.
        // Parse it again so a paragraph inside the frame stays a paragraph.
        // The iframe tag and its src are never copied.
        "iframe" => iframe_fallback(node, depth),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = name.as_bytes()[1].saturating_sub(b'0');
            vec![Block::Heading {
                level,
                inlines: collect_inlines(node, depth),
            }]
        }
        "p" => paragraph(collect_inlines(node, depth)),
        "blockquote" => vec![Block::Quote {
            blocks: children_of(node, depth),
        }],
        // Thunderbird writes its signature as `<pre class="moz-signature">`.
        // It is the sender's text, not code.
        "pre"
            if matches!(
                node.value().as_element().and_then(crate::history::marker),
                Some(crate::history::Marker::Signature)
            ) =>
        {
            crate::history::signature_container(pre_lines(node))
        }
        "pre" => vec![Block::Code {
            text: visible(&raw_text(node)).trim_matches('\n').to_string(),
            large: false,
        }],
        "ul" => vec![list_block(node, false, depth)],
        "ol" => vec![list_block(node, true, depth)],
        "hr" => vec![Block::ThematicBreak],
        "img" => node
            .value()
            .as_element()
            .filter(|element| !is_tracking_pixel(element) && !is_decorative(element))
            .map(image_block)
            .into_iter()
            .collect(),
        "table" => table_blocks(node, depth),
        "br" => Vec::new(),
        _ => {
            match node.value().as_element().and_then(crate::history::marker) {
                Some(crate::history::Marker::Quote) => {
                    return crate::history::quote_container(children_or_paragraph(node, depth));
                }
                Some(crate::history::Marker::Signature) => {
                    return crate::history::signature_container(children_or_paragraph(node, depth));
                }
                Some(crate::history::Marker::ReplyHeader) => {
                    let blocks = children_or_paragraph(node, depth);
                    if blocks.is_empty() {
                        return blocks;
                    }
                    return vec![Block::ReplyHeader { blocks }];
                }
                None => {}
            }
            if has_block_child(node, depth) {
                children_of(node, depth)
            } else {
                paragraph(collect_inlines(node, depth))
            }
        }
    }
}

fn iframe_fallback(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Block> {
    if depth >= MAX_HTML_DEPTH {
        return text_blocks(node);
    }
    let fallback = raw_text(node);
    if fallback.is_empty() {
        return Vec::new();
    }
    native_blocks(&fallback, depth)
}

fn children_or_paragraph(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Block> {
    if has_block_child(node, depth) {
        children_of(node, depth)
    } else {
        paragraph(collect_inlines(node, depth))
    }
}

fn paragraph(inlines: Vec<Inline>) -> Vec<Block> {
    if inlines.is_empty() {
        Vec::new()
    } else {
        vec![Block::Paragraph { inlines }]
    }
}

/// The lines of a `pre` as one paragraph, for a signature written in one.
fn pre_lines(node: ego_tree::NodeRef<'_, Node>) -> Vec<Block> {
    let text = visible(&raw_text(node));
    let mut inlines = Vec::new();
    for (at, line) in text.trim_matches('\n').lines().enumerate() {
        if at > 0 {
            inlines.push(Inline::LineBreak);
        }
        if line == SIGNATURE_DELIMITER.trim_end() || line == SIGNATURE_DELIMITER {
            inlines.push(Inline::Text(SIGNATURE_DELIMITER.into()));
        } else if !line.trim().is_empty() {
            inlines.push(Inline::Text(line.trim().to_string()));
        }
    }
    paragraph(inlines)
}

/// The blocks of an element with block children. Text and inline elements
/// between them join one paragraph, as a browser's anonymous block does, so
/// `Keep <code>Compact</code> behind a setting.<ol>…` stays one sentence.
///
/// A `<div>` that holds only inline content is a line, as a browser lays it
/// out: Gmail writes every line of a paragraph as its own `<div>`, and
/// Outlook every line as a margin-less `<p class="MsoNormal">`. A run of
/// them is one paragraph of lines. A blank one (`<div><br></div>`,
/// `<p class="MsoNormal">&nbsp;</p>`) is the gap between paragraphs.
fn children_of(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Block> {
    let mut lines = Lines::default();
    let mut run = Vec::new();
    for child in node.children() {
        if is_inline_node(child, depth + 1) {
            push_inline(child, depth + 1, &mut run);
            continue;
        }
        lines.end_run(&mut run);
        match line_kind(child, depth + 1) {
            LineKind::Line(inlines) => lines.line(inlines),
            LineKind::Spacer => lines.gap(),
            LineKind::Nothing => {}
            LineKind::Block => {
                lines.gap();
                lines.blocks.extend(node_blocks(child, depth + 1));
            }
        }
    }
    lines.end_run(&mut run);
    lines.gap();
    let mut blocks = merge_image_runs(lines.blocks);
    blocks.retain(|block| !block_is_empty(block));
    blocks
}

/// A paragraph built a line at a time.
#[derive(Default)]
struct Lines {
    blocks: Vec<Block>,
    paragraph: Vec<Inline>,
}

impl Lines {
    fn line(&mut self, inlines: Vec<Inline>) {
        if inlines.is_empty() {
            return;
        }
        if !self.paragraph.is_empty() {
            self.paragraph.push(Inline::LineBreak);
        }
        self.paragraph.extend(inlines);
    }

    /// End the paragraph. One that is only an image is that image's block.
    fn gap(&mut self) {
        let mut inlines = std::mem::take(&mut self.paragraph);
        if inlines.is_empty() {
            return;
        }
        if let [Inline::Image { .. }] = inlines.as_slice() {
            let Some(Inline::Image { source, alt }) = inlines.pop() else {
                unreachable!("matched one image");
            };
            self.blocks.push(Block::Image { source, alt });
            return;
        }
        self.blocks.push(Block::Paragraph { inlines });
    }

    /// Text and inline elements between blocks are a line. A run that is
    /// only `<br>`s is a blank line: a paragraph gap.
    fn end_run(&mut self, run: &mut Vec<Inline>) {
        let mut inlines = std::mem::take(run);
        let breaks = inlines
            .iter()
            .any(|inline| matches!(inline, Inline::LineBreak));
        finish_inlines(&mut inlines);
        if inlines.is_empty() {
            if breaks {
                self.gap();
            }
            return;
        }
        self.line(inlines);
    }
}

enum LineKind {
    Line(Vec<Inline>),
    Spacer,
    Nothing,
    Block,
}

/// How a block-level child joins the lines around it.
fn line_kind(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> LineKind {
    let Some(element) = node.value().as_element() else {
        return LineKind::Block;
    };
    if depth >= MAX_HTML_DEPTH {
        return LineKind::Block;
    }
    let name = element.name();
    if !matches!(name, "div" | "p")
        || crate::history::marker(element).is_some()
        || has_block_child(node, depth)
    {
        return LineKind::Block;
    }
    let inlines = collect_inlines(node, depth);
    let blank = inlines.iter().all(|inline| match inline {
        Inline::Text(text) => is_blank_text(text),
        Inline::LineBreak => true,
        _ => false,
    });
    if blank {
        return if has_break_or_nbsp(node, depth) {
            LineKind::Spacer
        } else if name == "div" {
            LineKind::Nothing
        } else {
            LineKind::Block
        };
    }
    let mso = element
        .classes()
        .any(|class| class.eq_ignore_ascii_case("MsoNormal"));
    if (name == "p" && !mso)
        || is_fine_print(element)
        || is_mono(element)
        || is_bold(element)
        || heading_level(node).is_some()
    {
        return LineKind::Block;
    }
    LineKind::Line(inlines)
}

/// `<br>` or `&nbsp;` somewhere inside: a blank line that takes room.
fn has_break_or_nbsp(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> bool {
    if depth >= MAX_HTML_DEPTH {
        return false;
    }
    node.children().any(|child| match child.value() {
        Node::Text(text) => text.contains('\u{a0}'),
        Node::Element(element) if element.name() == "br" => true,
        Node::Element(_) => has_break_or_nbsp(child, depth + 1),
        _ => false,
    })
}

/// Blocked remote images next to each other (a logo over a banner, a row
/// of social icons each in its own cell) join one paragraph, so the painter
/// draws them as one `Images blocked · …` line.
fn merge_image_runs(blocks: Vec<Block>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    let mut run: Vec<Inline> = Vec::new();
    let mut count = 0usize;
    let mut first: Option<Block> = None;
    let flush = |out: &mut Vec<Block>,
                 run: &mut Vec<Inline>,
                 count: &mut usize,
                 first: &mut Option<Block>| {
        match *count {
            0 => {}
            1 => out.extend(first.take()),
            _ => out.push(Block::Paragraph {
                inlines: std::mem::take(run),
            }),
        }
        run.clear();
        *count = 0;
        *first = None;
    };
    for block in blocks {
        let inlines = match &block {
            Block::Image {
                source: source @ ImageSource::Remote(_),
                alt,
            } => Some(vec![Inline::Image {
                source: source.clone(),
                alt: alt.clone(),
            }]),
            Block::Paragraph { inlines } if is_image_row(inlines) => Some(inlines.clone()),
            _ => None,
        };
        match inlines {
            Some(inlines) => {
                if !run.is_empty() {
                    run.push(Inline::Text(" ".into()));
                }
                run.extend(inlines);
                count += 1;
                if first.is_none() {
                    first = Some(block);
                }
            }
            None => {
                flush(&mut out, &mut run, &mut count, &mut first);
                out.push(block);
            }
        }
    }
    flush(&mut out, &mut run, &mut count, &mut first);
    out
}

/// Only remote images, maybe linked, with nothing but space between them.
pub(crate) fn is_image_row(inlines: &[Inline]) -> bool {
    fn walk(inlines: &[Inline], images: &mut usize) -> bool {
        inlines.iter().all(|inline| match inline {
            Inline::Image {
                source: ImageSource::Remote(_),
                ..
            } => {
                *images += 1;
                true
            }
            Inline::Link { children, .. } => walk(children, images),
            Inline::Text(text) => is_blank_text(text),
            Inline::LineBreak => true,
            _ => false,
        })
    }
    let mut images = 0;
    walk(inlines, &mut images) && images > 0
}

/// Lines quoted with `>` in mail written as lines and `<br>`s, as plain
/// text clients and some webmail reply: a run of them is a quote, and
/// `>>` a quote inside it. One `>` line on its own is the sender's text
/// unless an `… wrote:` line introduces it.
fn quote_marked_lines(blocks: Vec<Block>) -> Vec<Block> {
    quote_lines(blocks, false)
}

/// `nested`: inside a quote this pass made, where one `>` line is a
/// quote too.
fn quote_lines(blocks: Vec<Block>, nested: bool) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    // The last block out is a quote this pass made, which the next
    // paragraph's quoted lines continue.
    let mut open_quote = false;
    let mut after_attribution = false;
    for block in blocks {
        let Block::Paragraph { inlines } = block else {
            open_quote = false;
            after_attribution = false;
            out.push(block);
            continue;
        };
        let lines: Vec<&[Inline]> = inlines
            .split(|inline| matches!(inline, Inline::LineBreak))
            .collect();
        let marked: Vec<bool> = lines
            .iter()
            .map(|line| quote_mark(line).is_some())
            .collect();
        let count = marked.iter().filter(|marked| **marked).count();
        let introduced = |at: usize| {
            let before = if at == 0 {
                after_attribution
            } else {
                inlines_plain(lines[at - 1]).trim_end().ends_with("wrote:")
            };
            before || open_quote || nested
        };
        if count == 0
            || (count == 1
                && !marked
                    .iter()
                    .enumerate()
                    .any(|(at, m)| *m && introduced(at)))
        {
            after_attribution = inlines_plain(lines[lines.len() - 1])
                .trim_end()
                .ends_with("wrote:");
            open_quote = false;
            out.push(Block::Paragraph { inlines });
            continue;
        }
        let mut at = 0;
        while at < lines.len() {
            let start = at;
            let quoted = marked[at];
            while at < lines.len() && marked[at] == quoted {
                at += 1;
            }
            let run = &lines[start..at];
            if quoted {
                let inner = quote_lines(
                    lines_to_paragraphs(run.iter().map(|line| strip_quote_mark(line)).collect()),
                    true,
                );
                match out.last_mut() {
                    Some(Block::Quote { blocks }) if open_quote => blocks.extend(inner),
                    _ => out.push(Block::Quote { blocks: inner }),
                }
                open_quote = true;
            } else {
                let mut paragraph = Vec::new();
                for (index, line) in run.iter().enumerate() {
                    if index > 0 {
                        paragraph.push(Inline::LineBreak);
                    }
                    paragraph.extend_from_slice(line);
                }
                out.push(Block::Paragraph { inlines: paragraph });
                open_quote = false;
            }
        }
        after_attribution = false;
    }
    out
}

/// Where a line's `>` mark is: the first inline is text that starts with it.
fn quote_mark(line: &[Inline]) -> Option<()> {
    match line.first() {
        Some(Inline::Text(text)) if text.trim_start().starts_with('>') => Some(()),
        _ => None,
    }
}

/// The line without its first `>` and the space after it.
fn strip_quote_mark(line: &[Inline]) -> Vec<Inline> {
    let mut out = line.to_vec();
    if let Some(Inline::Text(text)) = out.first_mut() {
        let rest = text.trim_start().strip_prefix('>').unwrap_or(text);
        *text = rest.strip_prefix(' ').unwrap_or(rest).to_string();
        if text.is_empty() {
            out.remove(0);
        }
    }
    out
}

/// Lines into paragraphs, a blank line between two.
fn lines_to_paragraphs(lines: Vec<Vec<Inline>>) -> Vec<Block> {
    let mut out = Vec::new();
    let mut current: Vec<Inline> = Vec::new();
    for line in lines {
        let blank = line.iter().all(|inline| match inline {
            Inline::Text(text) => text.trim().is_empty(),
            _ => false,
        });
        if blank {
            if !current.is_empty() {
                out.push(Block::Paragraph {
                    inlines: std::mem::take(&mut current),
                });
            }
            continue;
        }
        if !current.is_empty() {
            current.push(Inline::LineBreak);
        }
        current.extend(line);
    }
    if !current.is_empty() {
        out.push(Block::Paragraph { inlines: current });
    }
    out
}

/// Drop a blocked image whose `Image blocked` line would only repeat what
/// the reader sees anyway: a logo named for the sender (`alt="Bayline
/// Ferries"` in mail from Bayline Ferries), or a banner whose alt text
/// starts with the heading right under it.
pub(crate) fn drop_named_images(document: &mut StyledDocument, sender: &str) {
    let normal = |text: &str| {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let sender = normal(sender);
    let blocks = std::mem::take(&mut document.blocks);
    let mut out = Vec::with_capacity(blocks.len());
    for (at, block) in blocks.iter().enumerate() {
        let redundant = lone_remote_alt(block).is_some_and(|alt| {
            let alt = normal(alt);
            if alt.is_empty() {
                return false;
            }
            let heading = blocks[at + 1..]
                .iter()
                .find(|next| !block_is_empty(next))
                .and_then(|next| match next {
                    Block::Heading { inlines, .. } => Some(normal(&inlines_plain(inlines))),
                    _ => None,
                });
            (!sender.is_empty() && alt == sender)
                || heading.is_some_and(|heading| !heading.is_empty() && alt.starts_with(&heading))
        });
        if !redundant {
            out.push(block.clone());
        }
    }
    document.blocks = out;
}

/// The alt text of a block that is one remote image, linked or not.
fn lone_remote_alt(block: &Block) -> Option<&str> {
    fn inline(inline: &Inline) -> Option<&str> {
        match inline {
            Inline::Image {
                source: ImageSource::Remote(_),
                alt,
            } => Some(alt),
            Inline::Link { children, .. } => match children.as_slice() {
                [only] => inline_alt(only),
                _ => None,
            },
            _ => None,
        }
    }
    match block {
        Block::Image {
            source: ImageSource::Remote(_),
            alt,
        } => Some(alt),
        Block::Paragraph { inlines } => match inlines.as_slice() {
            [only] => inline(only),
            _ => None,
        },
        _ => None,
    }
}

fn inline_alt(inline: &Inline) -> Option<&str> {
    match inline {
        Inline::Image {
            source: ImageSource::Remote(_),
            alt,
        } => Some(alt),
        _ => None,
    }
}

/// A paragraph with an empty line in it (`<br><br>`) is two paragraphs.
fn split_blank_lines(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::Paragraph { inlines } => {
                let mut current: Vec<Inline> = Vec::new();
                let mut breaks = 0usize;
                for inline in inlines {
                    match inline {
                        Inline::LineBreak => breaks += 1,
                        Inline::Text(text)
                            if breaks > 0
                                && is_blank_text(&text)
                                && text != SIGNATURE_DELIMITER => {}
                        other => {
                            if breaks >= 2 && !current.is_empty() {
                                out.push(Block::Paragraph {
                                    inlines: std::mem::take(&mut current),
                                });
                            } else if breaks == 1 && !current.is_empty() {
                                current.push(Inline::LineBreak);
                            }
                            breaks = 0;
                            current.push(other);
                        }
                    }
                }
                if !current.is_empty() {
                    out.push(Block::Paragraph { inlines: current });
                }
            }
            Block::Quote { blocks } => out.push(Block::Quote {
                blocks: split_blank_lines(blocks),
            }),
            Block::Small { blocks } => out.push(Block::Small {
                blocks: split_blank_lines(blocks),
            }),
            Block::ReplyHeader { blocks } => out.push(Block::ReplyHeader {
                blocks: split_blank_lines(blocks),
            }),
            Block::List { ordered, items } => out.push(Block::List {
                ordered,
                items: items
                    .into_iter()
                    .map(|item| ListItem {
                        blocks: split_blank_lines(item.blocks),
                    })
                    .collect(),
            }),
            other => out.push(other),
        }
    }
    out
}

/// Text, or an element that lays out inline and holds no block.
fn is_inline_node(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> bool {
    if depth >= MAX_HTML_DEPTH {
        return false;
    }
    match node.value() {
        Node::Text(_) => true,
        Node::Element(element) => {
            let name = element.name();
            if is_block_tag(name)
                || matches!(
                    name,
                    "iframe" | "object" | "noscript" | "svg" | "html" | "body" | "head"
                )
            {
                return false;
            }
            if crate::history::marker(element).is_some() || is_fine_print(element) {
                return false;
            }
            !has_block_child(node, depth)
        }
        _ => false,
    }
}
fn list_block(node: ego_tree::NodeRef<'_, Node>, ordered: bool, depth: u32) -> Block {
    let mut items = Vec::new();
    for child in node.children() {
        let Some(element) = child.value().as_element() else {
            continue;
        };
        if element.name() != "li" {
            continue;
        }
        let blocks = if has_block_child(child, depth + 1) {
            children_of(child, depth + 1)
        } else {
            paragraph(collect_inlines(child, depth + 1))
        };
        if !blocks.is_empty() {
            items.push(ListItem { blocks });
        }
    }
    Block::List { ordered, items }
}

/// One table cell: its blocks, the alignment it sets, and whether it is
/// a `<th>`.
struct Cell {
    blocks: Vec<Block>,
    align: Option<Align>,
    header: bool,
}

/// A data table stays a table. A layout table (`role="presentation"`, or
/// one with tables inside) is read row by row, its cells kept apart; but
/// one with no tables inside and at least two rows of two or more short
/// cells (a receipt's items and prices) reads as the data it is.
fn table_blocks(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Block> {
    let presentation = node.value().as_element().is_some_and(|element| {
        element.attr("role").is_some_and(|role| {
            role.eq_ignore_ascii_case("presentation") || role.eq_ignore_ascii_case("none")
        })
    });
    let nested = table_count(node, depth) > 1;
    let mut trs = Vec::new();
    table_rows(node, true, depth, &mut trs);
    let rows: Vec<Vec<Cell>> = trs
        .into_iter()
        .map(|tr| row_cells(tr, depth + 1))
        .filter(|row| !row.is_empty())
        .collect();
    if rows.is_empty() {
        return children_of(node, depth);
    }
    let columns = filled_columns(&rows);
    if columns < 2 || ((presentation || nested) && (nested || !reads_as_data(&rows))) {
        return merge_image_runs(linearize(rows));
    }
    data_table(rows)
}

/// Columns with something in them. A one-column table (an Outlook
/// signature, a boxed note) is text, not data.
fn filled_columns(rows: &[Vec<Cell>]) -> usize {
    let mut filled: Vec<bool> = Vec::new();
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            if filled.len() <= column {
                filled.resize(column + 1, false);
            }
            filled[column] |= !cell.blocks.is_empty();
        }
    }
    filled.into_iter().filter(|filled| *filled).count()
}

/// The `<tr>`s of this table, not of tables inside it.
fn table_rows<'a>(
    node: ego_tree::NodeRef<'a, Node>,
    root: bool,
    depth: u32,
    out: &mut Vec<ego_tree::NodeRef<'a, Node>>,
) {
    if depth >= MAX_HTML_DEPTH {
        return;
    }
    for child in node.children() {
        let Some(element) = child.value().as_element() else {
            continue;
        };
        match element.name() {
            "table" if !root => continue,
            "table" => {}
            "tr" => out.push(child),
            "td" | "th" => continue,
            _ => table_rows(child, false, depth + 1, out),
        }
    }
}

/// At least two rows with two or more short cells, and no long cell.
fn reads_as_data(rows: &[Vec<Cell>]) -> bool {
    let mut data_rows = 0;
    for row in rows {
        let filled: Vec<&Cell> = row.iter().filter(|cell| !cell.blocks.is_empty()).collect();
        if !filled.iter().all(|cell| short_cell(&cell.blocks)) {
            return false;
        }
        if filled.len() >= 2 {
            data_rows += 1;
        }
    }
    data_rows >= 2
}

fn short_cell(blocks: &[Block]) -> bool {
    match blocks {
        [Block::Paragraph { inlines }] => {
            !inlines
                .iter()
                .any(|inline| matches!(inline, Inline::LineBreak))
                && inlines_plain(inlines).chars().count() <= 60
        }
        _ => false,
    }
}

/// A layout table's rows, top to bottom. A row of one-line cells is one
/// line with the cells apart (`Logo · View online`), so no two cells run
/// together; cells holding more keep their blocks, one cell after another.
fn linearize(rows: Vec<Vec<Cell>>) -> Vec<Block> {
    let mut blocks = Vec::new();
    for row in rows {
        let cells: Vec<Vec<Block>> = row
            .into_iter()
            .map(|cell| cell.blocks)
            .filter(|blocks| !blocks.is_empty())
            .collect();
        if cells.len() >= 2 && cells.iter().all(|cell| one_line_cell(cell)) {
            let mut inlines = Vec::new();
            let mut previous_image = false;
            for cell in cells {
                let (cell_inlines, image) = match cell.into_iter().next() {
                    Some(Block::Paragraph { inlines }) => {
                        let image = is_image_row(&inlines);
                        (inlines, image)
                    }
                    Some(Block::Image { source, alt }) => {
                        let image = matches!(source, ImageSource::Remote(_));
                        (vec![Inline::Image { source, alt }], image)
                    }
                    _ => continue,
                };
                if !inlines.is_empty() {
                    // Images side by side are one blocked line; text cells
                    // read as items of a row.
                    let gap = if image && previous_image {
                        " "
                    } else {
                        " \u{b7} "
                    };
                    inlines.push(Inline::Text(gap.into()));
                }
                inlines.extend(cell_inlines);
                previous_image = image;
            }
            blocks.push(Block::Paragraph { inlines });
        } else {
            for cell in cells {
                blocks.extend(cell);
            }
        }
    }
    blocks
}

fn one_line_cell(blocks: &[Block]) -> bool {
    match blocks {
        [Block::Paragraph { inlines }] => !inlines
            .iter()
            .any(|inline| matches!(inline, Inline::LineBreak)),
        [Block::Image { .. }] => true,
        _ => false,
    }
}

/// Rows as a table. Each column takes the first alignment a cell in it
/// sets, and a column with nothing in any row is dropped.
fn data_table(rows: Vec<Vec<Cell>>) -> Vec<Block> {
    let mut header: Vec<Cell> = Vec::new();
    let mut body: Vec<Vec<Cell>> = Vec::new();
    for row in rows {
        let all_header = row.iter().all(|cell| cell.header);
        if all_header && header.is_empty() && body.is_empty() {
            header = row;
        } else {
            body.push(row);
        }
    }
    let columns = body
        .iter()
        .map(Vec::len)
        .chain(std::iter::once(header.len()))
        .max()
        .unwrap_or(0);
    let mut align = vec![None; columns];
    let mut filled = vec![false; columns];
    for row in std::iter::once(&header).chain(body.iter()) {
        for (column, cell) in row.iter().enumerate() {
            if align[column].is_none() {
                align[column] = cell.align;
            }
            filled[column] |= !cell.blocks.is_empty();
        }
    }
    let keep = |row: Vec<Cell>| -> Vec<Vec<Block>> {
        row.into_iter()
            .enumerate()
            .filter(|(column, _)| filled[*column])
            .map(|(_, cell)| cell.blocks)
            .collect()
    };
    let align = align
        .into_iter()
        .zip(&filled)
        .filter(|(_, filled)| **filled)
        .map(|(align, _)| align.unwrap_or_default())
        .collect();
    let header = keep(header);
    let rows: Vec<Vec<Vec<Block>>> = body.into_iter().map(keep).collect();
    if header.is_empty() && rows.iter().all(Vec::is_empty) {
        return Vec::new();
    }
    vec![Block::Table {
        header,
        rows,
        align,
    }]
}

fn cell_align(element: &scraper::node::Element) -> Option<Align> {
    let styled = style_value(element, "text-align");
    let value = styled.or_else(|| {
        element
            .attr("align")
            .map(|value| value.trim().to_ascii_lowercase())
    })?;
    match value.as_str() {
        "left" | "start" | "justify" => Some(Align::Start),
        "center" | "middle" => Some(Align::Center),
        "right" | "end" => Some(Align::End),
        _ => None,
    }
}

fn row_cells(row: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Cell> {
    row.children()
        .filter_map(|cell| {
            let element = cell.value().as_element()?;
            let name = element.name();
            if name != "td" && name != "th" {
                return None;
            }
            let blocks = styled_blocks(cell, element, cell_blocks(cell, depth));
            Some(Cell {
                blocks,
                align: cell_align(element),
                header: name == "th",
            })
        })
        .collect()
}

/// A cell keeps its paragraphs, lists, and quotes. Inlines are only for a cell
/// that has no block children, where a `<br>` is still a line break.
fn cell_blocks(cell: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Block> {
    let blocks = if has_block_child(cell, depth) {
        children_of(cell, depth)
    } else {
        paragraph(collect_inlines(cell, depth))
    };
    blocks
        .into_iter()
        .filter(|block| !block_is_empty(block))
        .collect()
}

fn table_count(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> usize {
    let here = node
        .value()
        .as_element()
        .is_some_and(|element| element.name() == "table") as usize;
    if depth >= MAX_HTML_DEPTH {
        return here;
    }
    here + node
        .children()
        .map(|child| table_count(child, depth + 1))
        .sum::<usize>()
}

fn has_block_child(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> bool {
    if depth >= MAX_HTML_DEPTH {
        return false;
    }
    // Fine print and history containers are blocks of their own, so a row
    // of plain cells does not swallow them into one line of inlines.
    node.children().any(|child| {
        child.value().as_element().is_some_and(|element| {
            is_block_tag(element.name())
                || is_fine_print(element)
                || crate::history::marker(element).is_some()
        }) || has_block_child(child, depth + 1)
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

/// A block's inlines, with spaces collapsed as a browser lays them out.
fn collect_inlines(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Inline> {
    let mut out = inline_children(node, depth);
    finish_inlines(&mut out);
    out
}

/// An inline element's children as they are: spaces at its edges belong
/// to the sentence around it (`foo<b> bar</b>`).
fn inline_children(node: ego_tree::NodeRef<'_, Node>, depth: u32) -> Vec<Inline> {
    let mut out = Vec::new();
    for child in node.children() {
        push_inline(child, depth + 1, &mut out);
    }
    out
}

fn push_inline(node: ego_tree::NodeRef<'_, Node>, depth: u32, out: &mut Vec<Inline>) {
    if depth >= MAX_HTML_DEPTH {
        let text = normalize_ws(&raw_text(node));
        let text = text.trim_matches(is_ascii_html_ws);
        if !text.is_empty() {
            out.push(Inline::Text(text.to_string()));
        }
        return;
    }
    match node.value() {
        Node::Text(text) if is_signature_delimiter(text.deref()) => {
            out.push(Inline::Text(SIGNATURE_DELIMITER.to_string()));
        }
        Node::Text(text) => {
            let text = normalize_ws(text.deref());
            if !text.is_empty() {
                out.push(Inline::Text(text));
            }
        }
        Node::Element(element) => match element.name() {
            "script" | "style" | "input" | "button" | "textarea" | "head" | "meta" | "link"
            | "title" | "embed" => {}
            "img" if is_tracking_pixel(element) || is_decorative(element) => {}
            "br" => out.push(Inline::LineBreak),
            "a" => out.push(Inline::Link {
                href: safe_href(element.attr("href").unwrap_or("")),
                children: inline_children(node, depth),
            }),
            "em" | "i" => out.push(Inline::Emphasis(inline_children(node, depth))),
            "strong" | "b" => out.push(Inline::Strong(inline_children(node, depth))),
            "s" | "del" | "strike" => out.push(Inline::Strike(inline_children(node, depth))),
            "code" | "tt" | "kbd" | "samp" => push_code(node, out),
            "img" => out.push(image_inline(element)),
            _ if is_mono(element) => push_code(node, out),
            _ => {
                let mut children = inline_children(node, depth);
                if is_strike(element) {
                    children = vec![Inline::Strike(children)];
                }
                if is_italic(element) {
                    children = vec![Inline::Emphasis(children)];
                }
                if is_bold(element) {
                    children = vec![Inline::Strong(children)];
                }
                out.extend(children);
            }
        },
        _ => {}
    }
}

fn push_code(node: ego_tree::NodeRef<'_, Node>, out: &mut Vec<Inline>) {
    let code = visible(&raw_text(node));
    let code = code.trim();
    if !code.is_empty() {
        out.push(Inline::Code(code.to_string()));
    }
}

/// Collapse spaces as a browser does: one space between words whatever
/// element each sits in, none at a block's edges or either side of a
/// `<br>`. The `-- ` signature delimiter keeps its space. Empty text and
/// empty wrappers go, and so do `<br>`s at the start or end.
pub(crate) fn finish_inlines(inlines: &mut Vec<Inline>) {
    fn forward(inlines: &mut [Inline], drop: &mut bool) {
        for inline in inlines {
            match inline {
                Inline::Text(text) if text == SIGNATURE_DELIMITER => *drop = true,
                Inline::Text(text) => {
                    if *drop {
                        let trimmed = text.trim_start_matches(' ');
                        if trimmed.len() != text.len() {
                            *text = trimmed.to_string();
                        }
                    }
                    if !text.is_empty() {
                        *drop = text.ends_with(' ');
                    }
                }
                Inline::LineBreak => *drop = true,
                Inline::Code(_) | Inline::Image { .. } => *drop = false,
                Inline::Emphasis(children)
                | Inline::Strong(children)
                | Inline::Strike(children)
                | Inline::Link { children, .. } => forward(children, drop),
            }
        }
    }
    fn backward(inlines: &mut [Inline], drop: &mut bool) {
        for inline in inlines.iter_mut().rev() {
            match inline {
                Inline::Text(text) if text == SIGNATURE_DELIMITER => *drop = false,
                Inline::Text(text) => {
                    if *drop {
                        let trimmed = text.trim_end_matches(' ');
                        if trimmed.len() != text.len() {
                            *text = trimmed.to_string();
                        }
                    }
                    if !text.is_empty() {
                        *drop = false;
                    }
                }
                Inline::LineBreak => *drop = true,
                Inline::Code(_) | Inline::Image { .. } => *drop = false,
                Inline::Emphasis(children)
                | Inline::Strong(children)
                | Inline::Strike(children)
                | Inline::Link { children, .. } => backward(children, drop),
            }
        }
    }
    fn prune(inlines: &mut Vec<Inline>) {
        inlines.retain_mut(|inline| match inline {
            Inline::Text(text) => !text.is_empty(),
            Inline::Emphasis(children)
            | Inline::Strong(children)
            | Inline::Strike(children)
            | Inline::Link { children, .. } => {
                prune(children);
                !children.is_empty()
            }
            Inline::Code(_) | Inline::Image { .. } | Inline::LineBreak => true,
        });
    }
    forward(inlines, &mut true);
    backward(inlines, &mut true);
    prune(inlines);
    while matches!(inlines.first(), Some(Inline::LineBreak)) {
        inlines.remove(0);
    }
    while matches!(inlines.last(), Some(Inline::LineBreak)) {
        inlines.pop();
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
    let alt = normalize_ws(&visible(element.attr("alt").unwrap_or("")))
        .trim()
        .to_string();
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
        return remote_data_uri(rest);
    };
    let media_type = meta
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .to_string();
    let base64 = meta.to_ascii_lowercase().contains("base64");
    if !base64 {
        if data.len() > crate::MAX_DATA_IMAGE_BYTES {
            return placeholder_image(media_type);
        }
        return ImageSource::Data {
            media_type,
            bytes: data.as_bytes().to_vec(),
        };
    }
    // RFC 2397 percent-decodes the payload before the base64 step, so `%2B`
    // and `%2F` are `+` and `/`. Whitespace, including a percent-encoded
    // newline, is stripped after that. The decoded length is checked before
    // the output buffer is allocated.
    match collect_base64_payload(data) {
        Err(PayloadLimit::TooLarge) => placeholder_image(media_type),
        Ok(payload) => match decode_base64_payload(&payload) {
            Some(bytes) => ImageSource::Data { media_type, bytes },
            None if rest.len() > crate::MAX_DATA_IMAGE_BYTES => placeholder_image(media_type),
            None => remote_data_uri(rest),
        },
    }
}

fn remote_data_uri(rest: &str) -> ImageSource {
    if rest.len() > crate::MAX_DATA_IMAGE_BYTES {
        return placeholder_image("application/octet-stream".to_string());
    }
    ImageSource::Remote(format!("data:{rest}"))
}

fn placeholder_image(media_type: String) -> ImageSource {
    ImageSource::Data {
        media_type,
        bytes: Vec::new(),
    }
}

enum PayloadLimit {
    TooLarge,
}

/// Largest base64 input whose [`base64::decoded_len_estimate`] stays inside
/// [`crate::MAX_DATA_IMAGE_BYTES`].
fn max_base64_chars() -> usize {
    (crate::MAX_DATA_IMAGE_BYTES / 3) * 4
}

fn collect_base64_payload(data: &str) -> Result<Vec<u8>, PayloadLimit> {
    let max_chars = max_base64_chars();
    let mut payload = Vec::with_capacity(data.len().min(max_chars));
    let bytes = data.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let decoded = if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            let value = hex_nibble(bytes[index + 1]) << 4 | hex_nibble(bytes[index + 2]);
            index += 3;
            value
        } else {
            let value = bytes[index];
            index += 1;
            value
        };
        if decoded.is_ascii_whitespace() {
            continue;
        }
        if payload.len() >= max_chars {
            return Err(PayloadLimit::TooLarge);
        }
        payload.push(decoded);
    }
    Ok(payload)
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn decode_base64_payload(payload: &[u8]) -> Option<Vec<u8>> {
    let estimate = base64::decoded_len_estimate(payload.len());
    if estimate > crate::MAX_DATA_IMAGE_BYTES {
        return None;
    }
    let mut buf = vec![0u8; estimate];
    let written = base64::Engine::decode_slice(
        &base64::engine::general_purpose::STANDARD,
        payload,
        &mut buf,
    )
    .or_else(|_| {
        base64::Engine::decode_slice(
            &base64::engine::general_purpose::STANDARD_NO_PAD,
            payload,
            &mut buf,
        )
    })
    .ok()?;
    buf.truncate(written);
    Some(buf)
}

/// Link targets that may be opened: `http`, `https` and `mailto`, and
/// nothing else. Browsers drop ASCII tabs and newlines anywhere in a URL and
/// C0 controls and spaces at its ends, so `java\nscript:` is `javascript:`;
/// the scheme is read the same way before it is checked.
pub(crate) fn safe_href(href: &str) -> String {
    let cleaned: String = href
        .trim_matches(|ch: char| ch <= ' ')
        .chars()
        .filter(|ch| !matches!(ch, '\t' | '\n' | '\r'))
        .collect();
    let Some((scheme, _)) = cleaned.split_once(':') else {
        return String::new();
    };
    let scheme = scheme.to_ascii_lowercase();
    if matches!(scheme.as_str(), "http" | "https" | "mailto") {
        cleaned
    } else {
        String::new()
    }
}

/// The RFC 3676 signature delimiter, as the walk keeps it: two dashes and a
/// space. The space is what tells it from a sender's own `--`.
pub(crate) const SIGNATURE_DELIMITER: &str = "-- ";

/// A text node that is the delimiter line: `-- ` (or `--&nbsp;`), with only
/// a line ending after it.
fn is_signature_delimiter(raw: &str) -> bool {
    let line = raw
        .trim_start_matches(is_ascii_html_ws)
        .trim_end_matches(['\r', '\n']);
    line == "-- " || line == "--\u{a0}"
}

/// Characters that reorder or hide what is drawn: bidi overrides and
/// isolates, zero-width marks, and the joiners newsletters pad a preheader
/// with. The same set compose's `display_safe` strips. Letters of
/// right-to-left scripts are kept; only the invisible controls go.
fn is_invisible(ch: char) -> bool {
    matches!(
        ch,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{034F}'
            | '\u{00AD}'
            | '\u{061C}'
    ) || (ch.is_control() && !matches!(ch, '\t' | '\n' | '\r' | '\u{000c}'))
}

fn visible(text: &str) -> String {
    text.chars().filter(|ch| !is_invisible(*ch)).collect()
}

struct Declaration {
    name: String,
    value: String,
    important: bool,
}

/// Inline declarations only, in source order.
fn style_declarations(element: &scraper::node::Element) -> Vec<Declaration> {
    let Some(style) = element.attr("style") else {
        return Vec::new();
    };
    style
        .split(';')
        .filter_map(|declaration| {
            let (name, value) = declaration.split_once(':')?;
            let value = value.trim().to_ascii_lowercase();
            let priority = value
                .rsplit_once('!')
                .filter(|(_, marker)| marker.trim() == "important");
            Some(Declaration {
                name: name.trim().to_ascii_lowercase(),
                value: priority
                    .map_or(value.as_str(), |(value, _)| value.trim())
                    .to_string(),
                important: priority.is_some(),
            })
        })
        .collect()
}

/// A length of zero: `0`, `0px`, `0pt`, `0em`, `0%`.
fn is_zero_length(value: &str) -> bool {
    let number = value.trim_end_matches(|ch: char| ch.is_ascii_alphabetic() || ch == '%');
    number.parse::<f32>().is_ok_and(|n| n == 0.0)
}

/// Prune once before the native walk, including text used by code blocks
/// and the depth fallback. Zero font size hides inherited text but can
/// leave images and explicitly sized descendants visible.
fn prune_hidden(dom: &mut Html) {
    let mut todo = vec![(*dom.root_element(), false)];
    let mut remove = Vec::new();
    while let Some((node, inherited_zero)) = todo.pop() {
        match node.value() {
            Node::Element(element) => {
                let style = style_declarations(element);
                let zero = has_zero_font_size(element, &style, inherited_zero);
                if is_hidden(element, &style) || (zero && element.name() == "br") {
                    if matches!(element.name(), "td" | "th") {
                        // The table reader keeps an empty slot for hidden cells.
                        remove.extend(node.children().map(|child| child.id()));
                    } else {
                        remove.push(node.id());
                    }
                } else if element.name() != "iframe" {
                    // Iframe fallback is raw HTML and is pruned after parsing it.
                    todo.extend(node.children().map(|child| (child, zero)));
                }
            }
            Node::Text(_) if inherited_zero => remove.push(node.id()),
            _ => {}
        }
    }
    for id in remove {
        dom.tree.get_mut(id).unwrap().detach();
    }
}

fn is_hidden(element: &scraper::node::Element, style: &[Declaration]) -> bool {
    // mso-hide is Outlook-specific. Its ordinary HTML fallback is visible here.
    element.attr("hidden").is_some()
        || declared_value(style, "display") == Some("none")
        || declared_value(style, "visibility") == Some("hidden")
        || (declared_value(style, "max-height").is_some_and(is_zero_length)
            && style
                .iter()
                .filter_map(|declaration| {
                    let value = match declaration.name.as_str() {
                        "overflow-y" => declaration.value.as_str(),
                        "overflow" => declaration.value.split_ascii_whitespace().next_back()?,
                        _ => return None,
                    };
                    Some((declaration.important, value))
                })
                .max_by_key(|(important, _)| *important)
                .is_some_and(|(_, value)| matches!(value, "hidden" | "clip")))
}

fn has_zero_font_size(
    element: &scraper::node::Element,
    style: &[Declaration],
    inherited: bool,
) -> bool {
    let Some(size) = declared_value(style, "font-size") else {
        // Legacy HTML can set an absolute font size without a style.
        let absolute = element.name() == "font"
            && element
                .attr("size")
                .is_some_and(|size| matches!(size.trim(), "1" | "2" | "3" | "4" | "5" | "6" | "7"));
        return inherited && !absolute;
    };
    is_zero_length(size)
        || (inherited
            && (matches!(size, "inherit" | "unset" | "smaller" | "larger")
                || size.ends_with('%')
                || size.ends_with("em") && !size.ends_with("rem")))
}

fn declared_value<'a>(style: &'a [Declaration], property: &str) -> Option<&'a str> {
    style
        .iter()
        .filter(|declaration| declaration.name == property)
        .max_by_key(|declaration| declaration.important)
        .map(|declaration| declaration.value.as_str())
}

/// A font size in CSS pixels, from `px` or `pt`.
fn font_px(element: &scraper::node::Element) -> Option<f32> {
    let value = style_value(element, "font-size")?;
    if let Some(number) = value.strip_suffix("px") {
        return number.trim().parse().ok();
    }
    if let Some(number) = value.strip_suffix("pt") {
        return number.trim().parse::<f32>().ok().map(|pt| pt * 4.0 / 3.0);
    }
    None
}

/// Fine print set on the one element a block wraps:
/// `<p><span style="font-size:x-small">Legal</span></p>`.
fn fine_print_inside(node: ego_tree::NodeRef<'_, Node>) -> bool {
    let mut only = None;
    for child in node.children() {
        match child.value() {
            Node::Text(text) if is_blank_text(text) => {}
            Node::Element(_) if only.is_none() => only = Some(child),
            _ => return false,
        }
    }
    only.and_then(|child| child.value().as_element())
        .is_some_and(|element| !is_block_tag(element.name()) && is_fine_print(element))
}

/// Fine print: 12 px or smaller, as newsletters set their footer, or the
/// `small` and `x-small` keywords Outlook and Gmail write for it.
fn is_fine_print(element: &scraper::node::Element) -> bool {
    if style_value(element, "font-size")
        .is_some_and(|value| matches!(value.as_str(), "small" | "x-small" | "xx-small"))
    {
        return true;
    }
    font_px(element).is_some_and(|size| size > 0.0 && size <= 12.0)
}

fn style_value(element: &scraper::node::Element, property: &str) -> Option<String> {
    declared_value(&style_declarations(element), property).map(str::to_owned)
}

fn is_bold(element: &scraper::node::Element) -> bool {
    style_value(element, "font-weight").is_some_and(|value| {
        matches!(
            value.as_str(),
            "bold" | "bolder" | "600" | "700" | "800" | "900"
        )
    })
}

fn is_italic(element: &scraper::node::Element) -> bool {
    style_value(element, "font-style").is_some_and(|value| value == "italic")
}

fn is_strike(element: &scraper::node::Element) -> bool {
    ["text-decoration", "text-decoration-line"]
        .iter()
        .any(|property| style_value(element, property).is_some_and(|v| v.contains("line-through")))
}

/// Set in a monospace face: `font-family: Menlo, Consolas, monospace`, or
/// `<font face="Courier New">`.
fn is_mono(element: &scraper::node::Element) -> bool {
    let family = style_value(element, "font-family")
        .or_else(|| element.attr("face").map(str::to_ascii_lowercase));
    family.is_some_and(|family| {
        [
            "monospace",
            "menlo",
            "consolas",
            "courier",
            "monaco",
            "sfmono",
        ]
        .iter()
        .any(|face| family.contains(face))
    })
}

/// An image the sender marked as decoration (`alt=""`): a hero banner, a
/// spacer, an icon beside its own label. A remote one never draws here,
/// so it is dropped rather than shown as `Image blocked`.
fn is_decorative(element: &scraper::node::Element) -> bool {
    let Some(alt) = element.attr("alt") else {
        return false;
    };
    let remote = element.attr("src").is_none_or(|src| {
        let src = src.trim_start();
        !src.starts_with("data:") && !src.starts_with("cid:")
    });
    remote && visible(alt).trim().is_empty()
}

/// An image 2 px or smaller on a side: an open-tracking pixel or a spacer.
fn is_tracking_pixel(element: &scraper::node::Element) -> bool {
    let attr = |name: &str| {
        element
            .attr(name)
            .and_then(|value| value.trim().trim_end_matches("px").parse::<f32>().ok())
    };
    let styled = |name: &str| {
        style_value(element, name)
            .and_then(|value| value.strip_suffix("px")?.trim().parse::<f32>().ok())
    };
    let width = attr("width").or_else(|| styled("width"));
    let height = attr("height").or_else(|| styled("height"));
    width.is_some_and(|w| w <= 2.0) || height.is_some_and(|h| h <= 2.0)
}

fn raw_text(node: ego_tree::NodeRef<'_, Node>) -> String {
    let mut out = String::new();
    for node in node.descendants() {
        match node.value() {
            Node::Text(text) => out.push_str(text.deref()),
            Node::Element(element) if element.name() == "br" => out.push('\n'),
            _ => {}
        }
    }
    out
}

fn is_ascii_html_ws(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{000c}')
}

fn normalize_ws(text: &str) -> String {
    let text = collapse_filler(&visible(text));
    let text = text.as_str();
    let mut out = String::new();
    let mut pending_space = false;
    if text.chars().next().is_some_and(is_ascii_html_ws) {
        out.push(' ');
    }
    for ch in text.chars() {
        if is_ascii_html_ws(ch) {
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
    // A text node of only spaces between two inline elements is one
    // space: `<s>$180</s> <b>$126</b>` reads `$180 $126`.
    if out.is_empty() && !text.is_empty() {
        out.push(' ');
    }
    out
}

/// Preheader filler (`&#847;&zwnj;&nbsp;` repeated) is a run of no-break
/// spaces once the invisible marks are gone. Three or more in a row are
/// padding, not spacing the sender typed, so they collapse like spaces.
fn collapse_filler(text: &str) -> String {
    if !text.contains("\u{a0}\u{a0}\u{a0}") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut run = 0usize;
    for ch in text.chars() {
        if ch == '\u{a0}' {
            run += 1;
            continue;
        }
        push_nbsp_run(&mut out, run);
        run = 0;
        out.push(ch);
    }
    push_nbsp_run(&mut out, run);
    out
}

fn push_nbsp_run(out: &mut String, run: usize) {
    match run {
        0 => {}
        1 | 2 => out.extend(std::iter::repeat_n('\u{a0}', run)),
        _ => out.push(' '),
    }
}

/// Blank to a reader: ASCII space or no-break space only.
fn is_blank_text(text: &str) -> bool {
    text.chars()
        .all(|ch| is_ascii_html_ws(ch) || ch == '\u{a0}')
}

fn block_is_empty(block: &Block) -> bool {
    match block {
        // `<div><br></div>` is a spacer: the painter already spaces blocks.
        // So is a paragraph of only `&nbsp;` (Outlook's spacer).
        Block::Heading { inlines, .. } | Block::Paragraph { inlines } => {
            inlines.iter().all(|inline| match inline {
                Inline::LineBreak => true,
                Inline::Text(text) => is_blank_text(text),
                _ => false,
            })
        }
        Block::Small { blocks } | Block::ReplyHeader { blocks } => {
            blocks.iter().all(block_is_empty)
        }
        Block::List { items, .. } => items.is_empty(),
        Block::Quote { blocks } => blocks.is_empty(),
        Block::Code { text, .. } => text.trim().is_empty(),
        Block::Table { header, rows, .. } => header.is_empty() && rows.is_empty(),
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
    fn a_trailing_line_break_draws_no_line() {
        let doc = html_to_document("<div>On Monday, Ada wrote:<br></div><p>One<br>Two<br> </p>");
        assert_eq!(
            doc.blocks[0],
            Block::Paragraph {
                inlines: vec![Inline::Text("On Monday, Ada wrote:".into())]
            }
        );
        let Block::Paragraph { inlines } = &doc.blocks[1] else {
            panic!("{:?}", doc.blocks);
        };
        assert_eq!(inlines.len(), 3, "{inlines:?}");
    }

    #[test]
    fn a_line_break_spacer_is_not_a_paragraph() {
        let doc = html_to_document(
            "<div dir=\"ltr\"><div>One</div><div><br></div><div> <br> </div><blockquote><div><br></div><p>Two</p></blockquote></div>",
        );
        assert_eq!(doc.blocks.len(), 2, "{:?}", doc.blocks);
        let Block::Quote { blocks } = &doc.blocks[1] else {
            panic!("{:?}", doc.blocks);
        };
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn simple_table_stays_a_table() {
        let doc = html_to_document(include_str!("../fixtures/native-personal-table.html"));
        let Block::Table { header, rows, .. } = doc
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

        for body in ["<p Hello there>", "<table for Thursday>"] {
            assert!(!looks_like_html(body), "{body}");
            let text = document_from_body(body).plain_text();
            assert!(
                text.contains("Hello") || text.contains("Thursday"),
                "{text}"
            );
            assert!(text.contains("there") || text.contains("for"), "{text}");
        }
        assert!(looks_like_html("<p>Hello</p>"));
        assert!(looks_like_html("<br/>"));
        assert!(looks_like_html("<p class=\"x\">Hello</p>"));
        assert!(looks_like_html("\u{feff}<p>Hello</p>"));
        assert!(looks_like_html("<!-- note --><p>Hello</p>"));
        assert!(!looks_like_html("<!-- note --> Hello there"));
        assert!(looks_like_html(
            "<!--[if mso]><table><tr><td>Hidden</td></tr></table><![endif]--><p>Visible</p>"
        ));
        let outlook = document_from_body("<!--[if mso]><p>Hidden</p><![endif]--><p>Visible</p>");
        assert!(outlook.plain_text().contains("Visible"), "{outlook:?}");
        assert!(!outlook.plain_text().contains("[if mso]"));
    }

    #[test]
    fn deep_nesting_keeps_the_text() {
        let mut html = String::new();
        for _ in 0..2_000 {
            html.push_str("<div>");
        }
        html.push_str("Still here");
        for _ in 0..2_000 {
            html.push_str("</div>");
        }
        let text = html_to_document(&html).plain_text();
        assert!(text.contains("Still here"), "{text}");

        let mut spans = String::from("<p>");
        for _ in 0..2_000 {
            spans.push_str("<span>");
        }
        spans.push_str("Inline");
        for _ in 0..2_000 {
            spans.push_str("</span>");
        }
        spans.push_str("</p>");
        let text = html_to_document(&spans).plain_text();
        assert!(text.contains("Inline"), "{text}");
    }

    #[test]
    fn iframe_and_object_keep_fallback_text_without_their_url() {
        let doc = html_to_document(
            r#"<iframe src="https://example.com/x"><p>Thursday at 3pm</p></iframe><object data="https://example.com/y"><p>Friday at 4</p></object>"#,
        );
        let text = doc.plain_text();
        assert!(text.contains("Thursday at 3pm"), "{text}");
        assert!(text.contains("Friday at 4"), "{text}");
        assert!(!text.contains("example.com"), "{text}");
        assert!(!text.contains("<iframe"), "{text}");
        assert!(!text.contains("<object"), "{text}");
    }

    #[test]
    fn pre_and_code_keep_a_newline_for_each_br() {
        let doc = html_to_document("<pre>a<br>b</pre><p><code>c<br>d</code></p>");
        let Block::Code { text, .. } = &doc.blocks[0] else {
            panic!("{:?}", doc.blocks);
        };
        assert_eq!(text, "a\nb");
        let Block::Paragraph { inlines } = &doc.blocks[1] else {
            panic!("{:?}", doc.blocks);
        };
        assert!(
            matches!(&inlines[0], Inline::Code(text) if text == "c\nd"),
            "{inlines:?}"
        );
    }

    #[test]
    fn percent_encoded_base64_plus_and_slash_decode() {
        let raw = [0xfb_u8, 0xef, 0xff];
        let payload = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
        assert!(payload.contains('+') && payload.contains('/'), "{payload}");
        for (plus, slash) in [("%2B", "%2F"), ("%2b", "%2f")] {
            let escaped = payload.replace('+', plus).replace('/', slash);
            let source = image_source(&format!("data:image/png;base64,{escaped}"));
            match source {
                ImageSource::Data { bytes, .. } => assert_eq!(bytes, raw),
                other => panic!("{plus} {slash} became {other:?}"),
            }
        }
    }

    #[test]
    fn data_uri_over_the_cap_keeps_no_bytes() {
        let payload = "A".repeat(max_base64_chars() + 1);
        let source = image_source(&format!("data:image/png;base64,{payload}"));
        match source {
            ImageSource::Data { bytes, .. } => assert!(bytes.is_empty(), "stored {}", bytes.len()),
            other => panic!("oversized payload became {other:?}"),
        }
        assert!(base64::decoded_len_estimate(max_base64_chars()) <= crate::MAX_DATA_IMAGE_BYTES);
        assert!(base64::decoded_len_estimate(max_base64_chars() + 1) > crate::MAX_DATA_IMAGE_BYTES);
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
        assert!(!text.contains("<p>"), "{text}");
        assert!(!text.contains("Go"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn nbsp_is_kept_and_ascii_space_collapses() {
        // Outlook's `<p>&nbsp;</p>` spacer is not a paragraph.
        let only =
            html_to_document("<p>&nbsp;&nbsp;</p><p>Hi</p><p class=\"MsoNormal\">&nbsp;</p>");
        assert_eq!(only.blocks.len(), 1, "{:?}", only.blocks);
        let mixed = html_to_document("<p>a&nbsp;b  c</p>");
        let text = mixed.plain_text();
        assert!(text.contains("a\u{00A0}b"), "{text:?}");
        assert!(text.contains("b c"), "{text:?}");
        assert!(!text.contains("b  c"), "{text:?}");
    }

    #[test]
    fn plain_text_is_paragraphs() {
        let doc = document_from_body("Hello\n\nNext");
        assert_eq!(doc.blocks.len(), 2);
        assert!(!looks_like_html("Hello"));
    }

    #[test]
    fn inline_code_stays_in_its_sentence() {
        let doc = html_to_document(
            "<ol><li>Keep <code>Compact</code> behind a setting.<ol><li>Only if asked.</li></ol></li></ol><div>Run <code>fell sync</code> then <b>wait</b>.<div>Next</div></div>",
        );
        let Block::List { items, .. } = &doc.blocks[0] else {
            panic!("{:?}", doc.blocks);
        };
        let Block::Paragraph { inlines } = &items[0].blocks[0] else {
            panic!("{:?}", items[0].blocks);
        };
        assert_eq!(
            inlines,
            &vec![
                Inline::Text("Keep ".into()),
                Inline::Code("Compact".into()),
                Inline::Text(" behind a setting.".into()),
            ]
        );
        assert!(matches!(items[0].blocks[1], Block::List { .. }));
        // `Next` is the next line of the same paragraph, as a browser
        // lays out a `<div>` of inline text.
        assert_eq!(doc.blocks.len(), 2, "{:?}", doc.blocks);
        let Block::Paragraph { inlines } = &doc.blocks[1] else {
            panic!("{:?}", doc.blocks);
        };
        assert_eq!(inlines.len(), 7, "{inlines:?}");
        assert_eq!(inlines[5], Inline::LineBreak);
    }

    #[test]
    fn hidden_preheaders_and_filler_never_show() {
        let doc = html_to_document(concat!(
            "<span style=\"display:none;max-height:0;overflow:hidden\">Preheader one&#847;&zwnj;&nbsp;&#847;&zwnj;&nbsp;</span>",
            "<div style=\"display: none !important\">Preheader two</div>",
            "<div style=\"max-height:0px;overflow:hidden\">Preheader three</div>",
            "<div style=\"font-size:0\">Preheader four</div>",
            "<div style=\"mso-hide:all;display:none\">Preheader five</div>",
            "<div style=\"visibility:hidden\">Preheader six</div>",
            "<div hidden>Preheader seven</div>",
            "<p>The week the river slowed.&#847;&zwnj;&nbsp;&#847;&zwnj;&nbsp;&#847;&zwnj;&nbsp;&#847;&zwnj;&nbsp;Next.</p>",
            "<p>&#847;&zwnj;&nbsp;&#847;&zwnj;&nbsp;&#847;&zwnj;&nbsp;</p>",
        ));
        let text = doc.plain_text();
        assert!(!text.contains("Preheader"), "{text:?}");
        assert_eq!(doc.blocks.len(), 1, "{:?}", doc.blocks);
        assert_eq!(text, "The week the river slowed. Next.");
    }

    #[test]
    fn bidi_overrides_and_zero_width_marks_are_dropped() {
        let doc = html_to_document(
            "<p>Invoice \u{202E}fdp.exe\u{202C} from a\u{200B}d\u{FEFF}a <a href=\"https://x.test\" title=\"t\">li\u{2066}nk\u{2069}</a></p><p>\u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{200F}</p><pre>co\u{202D}de</pre><p><code>c\u{200D}d</code><img src=\"cid:x\" alt=\"al\u{202E}t\"></p>",
        );
        let debug = format!("{doc:?}");
        for mark in [
            '\u{202E}', '\u{202C}', '\u{200B}', '\u{FEFF}', '\u{2066}', '\u{2069}', '\u{200F}',
            '\u{202D}', '\u{200D}',
        ] {
            assert!(!debug.contains(mark), "{mark:?} in {debug}");
        }
        let text = doc.plain_text();
        assert!(text.contains("Invoice fdp.exe from ada link"), "{text}");
        // Hebrew letters stay.
        assert!(text.contains("\u{5e9}\u{5dc}\u{5d5}\u{5dd}"), "{text}");
        assert!(text.contains("code"));
    }

    #[test]
    fn only_web_and_mail_links_keep_their_target() {
        for (href, kept) in [
            ("https://fell.test/a", true),
            ("  HTTP://fell.test/a ", true),
            ("mailto:ada@fell.test", true),
            ("javascript:alert(1)", false),
            ("java\nscript:alert(1)", false),
            ("java\tscript:alert(1)", false),
            ("\u{1}javascript:alert(1)", false),
            (" JaVaScRiPt:alert(1)", false),
            ("data:text/html,<script>x</script>", false),
            ("data:image/svg+xml,<svg onload=x>", false),
            ("vbscript:x", false),
            ("file:///etc/passwd", false),
            ("/relative", false),
            ("#top", false),
        ] {
            let target = safe_href(href);
            assert_eq!(!target.is_empty(), kept, "{href:?} -> {target:?}");
        }
        assert_eq!(safe_href("https://fell.test/a\nb"), "https://fell.test/ab");
    }

    #[test]
    fn tracking_pixels_go_and_fine_print_is_marked() {
        let doc = html_to_document(
            "<table role=\"presentation\"><tr><td style=\"font-size:11px\">View online</td></tr></table><p>Story</p><img src=\"https://t.test/o.gif\" width=\"1\" height=\"1\" alt=\"\"><p>Hi<img src=\"https://t.test/p.gif\" style=\"width:1px;height:1px\"></p><img src=\"https://t.test/logo.png\" width=\"180\" height=\"40\" alt=\"Logo\"><td><div style=\"font-size:11px;color:#999\">4 Quay Row<br><a href=\"https://x.test/u\">Unsubscribe</a></div></td><p style=\"font-size:8pt\">Legal</p><p style=\"font-size:16px\">Body</p>",
        );
        let debug = format!("{doc:?}");
        assert!(
            !debug.contains("o.gif") && !debug.contains("p.gif"),
            "{debug}"
        );
        assert!(debug.contains("logo.png"));
        let small: Vec<_> = doc
            .blocks
            .iter()
            .filter(|block| matches!(block, Block::Small { .. }))
            .collect();
        assert_eq!(small.len(), 3, "{:?}", doc.blocks);
        // A message set entirely in a small face is ordinary text.
        let all_small = html_to_document("<div style=\"font-size:12px\"><p>Hi</p><p>Bye</p></div>");
        assert!(
            matches!(all_small.blocks[0], Block::Paragraph { .. }),
            "{:?}",
            all_small.blocks
        );
    }

    #[test]
    fn table_cells_start_aligned_unless_the_column_says_otherwise() {
        let doc = html_to_document(
            "<table align=\"center\"><tr><th>Day</th><th align=\"right\">Count</th><th style=\"text-align: center\">Where</th></tr><tr><td>Sat</td><td>3</td><td>Pier</td></tr><tr><td>Sun</td><td>4</td><td align=\"right\">Square</td></tr></table>",
        );
        let Block::Table { align, .. } = &doc.blocks[0] else {
            panic!("{:?}", doc.blocks);
        };
        assert_eq!(align, &vec![Align::Start, Align::End, Align::Center]);
        let plain = html_to_document("<table><tr><td>a</td><td>b</td></tr></table>");
        let Block::Table { align, .. } = &plain.blocks[0] else {
            panic!("{:?}", plain.blocks);
        };
        assert_eq!(align, &vec![Align::Start, Align::Start]);
    }

    #[test]
    fn gt_quoted_lines_in_line_break_mail_are_a_quote() {
        let doc = html_to_document(
            "<div>Sounds good.<br><br>On Monday, Ada wrote:<br>&gt; Lunch on Thursday?<br>&gt;<br>&gt; &gt; Or Friday.<br>&gt; Either works.</div>",
        );
        assert_eq!(doc.blocks.len(), 3, "{:?}", doc.blocks);
        assert_eq!(doc.plain_text().lines().next(), Some("Sounds good."));
        let Block::Quote { blocks } = &doc.blocks[2] else {
            panic!("{:?}", doc.blocks);
        };
        assert!(matches!(blocks[0], Block::Paragraph { .. }));
        assert!(matches!(blocks[1], Block::Quote { .. }), "{blocks:?}");
        assert!(format!("{blocks:?}").contains("Either works."));
        // One `>` line the sender wrote stays theirs.
        let doc = html_to_document("<div>Score so far:<br>&gt; 3 wins</div>");
        assert!(matches!(doc.blocks[0], Block::Paragraph { .. }));
        assert_eq!(doc.blocks.len(), 1);
    }

    #[test]
    fn small_font_keywords_are_fine_print() {
        for size in ["small", "x-small", "XX-SMALL"] {
            let doc = html_to_document(&format!(
                "<p>Body</p><p style=\"font-size:{size}\">Legal</p>"
            ));
            assert!(matches!(doc.blocks[1], Block::Small { .. }), "{size}");
        }
        let doc = html_to_document("<p>Body</p><p style=\"font-size:medium\">More</p>");
        assert!(matches!(doc.blocks[1], Block::Paragraph { .. }));
    }

    #[test]
    fn a_code_set_large_is_marked_large() {
        let doc = html_to_document(
            "<p>Your code:</p><div style=\"font-family:monospace;font-size:28px\">482 913</div><div style=\"font-family:monospace\">ls -la</div>",
        );
        assert_eq!(
            doc.blocks[1],
            Block::Code {
                text: "482 913".into(),
                large: true
            }
        );
        assert_eq!(
            doc.blocks[2],
            Block::Code {
                text: "ls -la".into(),
                large: false
            }
        );
    }

    #[test]
    fn a_blocked_logo_named_for_the_sender_or_its_heading_goes() {
        let mut doc = html_to_document(
            "<img src=\"https://a.test/logo.png\" alt=\"Bayline  Ferries\"><h2>Booked</h2><a href=\"https://a.test\"><img src=\"https://a.test/b.png\" alt=\"Ridge Parka in moss\"></a><h3>Ridge Parka</h3><img src=\"https://a.test/c.png\" alt=\"Map of the route\"><p>Text</p>",
        );
        drop_named_images(&mut doc, "Bayline Ferries");
        let text = format!("{:?}", doc.blocks);
        assert!(!text.contains("logo.png"), "{text}");
        assert!(!text.contains("b.png"), "{text}");
        assert!(text.contains("Map of the route"), "{text}");
    }
}
