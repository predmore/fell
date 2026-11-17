/// Styled mail, independent of GPUI.
///
/// HTML mail and (later) the M3 composer both produce this tree. The native
/// view paints it; compose edits markdown and calls [`crate::markdown_to_document`]
/// so sent and received mail share one shape.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledDocument {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph {
        inlines: Vec<Inline>,
    },
    List {
        ordered: bool,
        items: Vec<ListItem>,
    },
    Quote {
        blocks: Vec<Block>,
    },
    Code {
        text: String,
    },
    /// Cells are blocks, so a paragraph break, list marker, or quote inside a
    /// cell survives. Flattening a cell to inlines glues `<p>Hello</p><p>World</p>`
    /// into `HelloWorld` and drops those markers.
    Table {
        header: Vec<Vec<Block>>,
        rows: Vec<Vec<Vec<Block>>>,
    },
    Image {
        source: ImageSource,
        alt: String,
    },
    ThematicBreak,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Code(String),
    Link { href: String, children: Vec<Inline> },
    Image { source: ImageSource, alt: String },
    LineBreak,
}

/// Where an image's bytes come from. Remote URLs are never fetched here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    Remote(String),
    Cid(String),
    Data { media_type: String, bytes: Vec<u8> },
}

impl StyledDocument {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            push_block(&mut out, block);
        }
        out.trim_matches(|ch: char| matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{000c}'))
            .to_string()
    }
}

fn push_block(out: &mut String, block: &Block) {
    match block {
        Block::Heading { inlines, .. } | Block::Paragraph { inlines } => {
            push_inlines(out, inlines);
            out.push('\n');
        }
        Block::List { items, .. } => {
            for item in items {
                for child in &item.blocks {
                    push_block(out, child);
                }
            }
        }
        Block::Quote { blocks } => {
            for child in blocks {
                push_block(out, child);
            }
        }
        Block::Code { text } => {
            out.push_str(text);
            out.push('\n');
        }
        Block::Table { header, rows } => {
            if !header.is_empty() {
                for cell in header {
                    push_blocks(out, cell);
                    out.push('\t');
                }
                out.push('\n');
            }
            for row in rows {
                for cell in row {
                    push_blocks(out, cell);
                    out.push('\t');
                }
                out.push('\n');
            }
        }
        Block::Image { alt, .. } => {
            if !alt.is_empty() {
                out.push_str(alt);
                out.push('\n');
            }
        }
        Block::ThematicBreak => out.push('\n'),
    }
}

fn push_blocks(out: &mut String, blocks: &[Block]) {
    for block in blocks {
        push_block(out, block);
    }
}

fn push_inlines(out: &mut String, inlines: &[Inline]) {
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) => out.push_str(text),
            Inline::Emphasis(children)
            | Inline::Strong(children)
            | Inline::Link { children, .. } => {
                push_inlines(out, children);
            }
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::LineBreak => out.push('\n'),
        }
    }
}
