use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::document::{Block, Inline, ListItem, StyledDocument};
use crate::html::image_source_from_markdown;

/// Markdown the composer will edit, turned into the same tree HTML mail uses.
/// Markers stay in the source; this only describes how they paint.
pub fn markdown_to_document(markdown: &str) -> StyledDocument {
    let parser = Parser::new_ext(markdown, Options::empty());
    let mut stack = vec![Frame::Blocks(Vec::new())];
    let mut code: Option<String> = None;

    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) => stack.push(Frame::Inlines(Vec::new())),
            Event::End(TagEnd::Paragraph) => finish_inlines(&mut stack, false, 0),
            Event::Start(Tag::Heading { .. }) => stack.push(Frame::Inlines(Vec::new())),
            Event::End(TagEnd::Heading(level)) => {
                finish_inlines(&mut stack, true, heading_level(level));
            }
            Event::Start(Tag::BlockQuote(_)) => stack.push(Frame::Blocks(Vec::new())),
            Event::End(TagEnd::BlockQuote(_)) => {
                if let Some(Frame::Blocks(blocks)) = stack.pop() {
                    push_block(&mut stack, Block::Quote { blocks });
                }
            }
            Event::Start(Tag::List(first)) => stack.push(Frame::List {
                ordered: first.is_some(),
                items: Vec::new(),
            }),
            Event::End(TagEnd::List(_)) => {
                if let Some(Frame::List { ordered, items }) = stack.pop() {
                    push_block(&mut stack, Block::List { ordered, items });
                }
            }
            Event::Start(Tag::Item) => stack.push(Frame::Item(Vec::new())),
            Event::End(TagEnd::Item) => {
                if let Some(Frame::Item(blocks)) = stack.pop()
                    && let Some(Frame::List { items, .. }) = stack.last_mut()
                {
                    items.push(ListItem { blocks });
                }
            }
            Event::Start(Tag::CodeBlock(_)) => code = Some(String::new()),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(text) = code.take() {
                    push_block(
                        &mut stack,
                        Block::Code {
                            text: text.trim_end_matches('\n').to_string(),
                        },
                    );
                }
            }
            Event::Start(Tag::Emphasis) => stack.push(Frame::Inlines(Vec::new())),
            Event::End(TagEnd::Emphasis) => finish_wrap(&mut stack, Wrap::Emphasis),
            Event::Start(Tag::Strong) => stack.push(Frame::Inlines(Vec::new())),
            Event::End(TagEnd::Strong) => finish_wrap(&mut stack, Wrap::Strong),
            Event::Start(Tag::Link { dest_url, .. }) => {
                stack.push(Frame::Link {
                    href: dest_url.to_string(),
                    children: Vec::new(),
                });
            }
            Event::End(TagEnd::Link) => finish_wrap(&mut stack, Wrap::Link),
            Event::Start(Tag::Image { dest_url, .. }) => {
                stack.push(Frame::Image {
                    src: dest_url.to_string(),
                    alt: String::new(),
                });
            }
            Event::End(TagEnd::Image) => {
                if let Some(Frame::Image { src, alt }) = stack.pop() {
                    push_inline(
                        &mut stack,
                        Inline::Image {
                            source: image_source_from_markdown(&src),
                            alt,
                        },
                    );
                }
            }
            Event::Text(text) => {
                if let Some(code) = code.as_mut() {
                    code.push_str(&text);
                } else if let Some(Frame::Image { alt, .. }) = stack.last_mut() {
                    alt.push_str(&text);
                } else {
                    push_inline(&mut stack, Inline::Text(text.to_string()));
                }
            }
            Event::Code(text) => push_inline(&mut stack, Inline::Code(text.to_string())),
            // CommonMark joins a wrapped line with a space. Two spaces before
            // the newline, or a backslash, is the hard break.
            Event::SoftBreak => push_inline(&mut stack, Inline::Text(" ".to_string())),
            Event::HardBreak => push_inline(&mut stack, Inline::LineBreak),
            Event::Rule => push_block(&mut stack, Block::ThematicBreak),
            Event::Html(html) | Event::InlineHtml(html) => {
                push_inline(&mut stack, Inline::Text(html.to_string()));
            }
            Event::Start(_)
            | Event::End(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    let blocks = match stack.pop() {
        Some(Frame::Blocks(blocks)) => blocks,
        _ => Vec::new(),
    };
    StyledDocument { blocks }
}

enum Frame {
    Blocks(Vec<Block>),
    Item(Vec<Block>),
    List { ordered: bool, items: Vec<ListItem> },
    Inlines(Vec<Inline>),
    Link { href: String, children: Vec<Inline> },
    Image { src: String, alt: String },
}

enum Wrap {
    Emphasis,
    Strong,
    Link,
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn finish_inlines(stack: &mut Vec<Frame>, heading: bool, level: u8) {
    let Some(Frame::Inlines(inlines)) = stack.pop() else {
        return;
    };
    let block = if heading {
        Block::Heading { level, inlines }
    } else {
        Block::Paragraph { inlines }
    };
    push_block(stack, block);
}

fn finish_wrap(stack: &mut Vec<Frame>, wrap: Wrap) {
    let inline = match stack.pop() {
        Some(Frame::Inlines(children)) if matches!(wrap, Wrap::Emphasis) => {
            Inline::Emphasis(children)
        }
        Some(Frame::Inlines(children)) if matches!(wrap, Wrap::Strong) => Inline::Strong(children),
        Some(Frame::Link { href, children }) => Inline::Link { href, children },
        _ => return,
    };
    push_inline(stack, inline);
}

fn push_block(stack: &mut [Frame], block: Block) {
    if let Some(Frame::Blocks(blocks) | Frame::Item(blocks)) = stack.last_mut() {
        blocks.push(block);
    }
}

fn push_inline(stack: &mut [Frame], inline: Inline) {
    if let Some(
        Frame::Inlines(inlines)
        | Frame::Link {
            children: inlines, ..
        },
    ) = stack.last_mut()
    {
        inlines.push(inline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Block, Inline};

    #[test]
    fn compose_markdown_uses_the_same_blocks_as_html() {
        let doc = markdown_to_document(
            "# Thursday\n\n**Bold** and *italic* and `code`\n\n- one\n- two\n\n> quoted\n\n[notes](https://example.com)\n\n![sketch](cid:sketch)\n",
        );
        assert!(
            doc.blocks
                .iter()
                .any(|block| matches!(block, Block::Heading { level: 1, .. }))
        );
        assert!(
            doc.blocks
                .iter()
                .any(|block| matches!(block, Block::List { ordered: false, .. }))
        );
        assert!(
            doc.blocks
                .iter()
                .any(|block| matches!(block, Block::Quote { .. }))
        );
        let text = doc.plain_text();
        assert!(text.contains("Bold"));
        assert!(text.contains("notes"));
        assert!(text.contains("sketch"));
    }

    #[test]
    fn soft_break_is_a_space_and_hard_break_is_a_line() {
        let soft = markdown_to_document("Hello\nworld\n");
        let Block::Paragraph { inlines } = &soft.blocks[0] else {
            panic!("{:?}", soft.blocks);
        };
        assert!(
            !inlines
                .iter()
                .any(|inline| matches!(inline, Inline::LineBreak)),
            "{inlines:?}"
        );
        assert_eq!(soft.plain_text(), "Hello world");

        let hard = markdown_to_document("Hello  \nworld\n");
        let Block::Paragraph { inlines } = &hard.blocks[0] else {
            panic!("{:?}", hard.blocks);
        };
        assert!(
            inlines
                .iter()
                .any(|inline| matches!(inline, Inline::LineBreak)),
            "{inlines:?}"
        );
        assert_eq!(hard.plain_text(), "Hello\nworld");
    }
}
