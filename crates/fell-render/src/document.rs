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
        /// The sender's first ordinal for an ordered list.
        start: u64,
        style: ListLabelStyle,
        items: Vec<ListItem>,
    },
    Quote {
        blocks: Vec<Block>,
    },
    Code {
        text: String,
        /// Set at 20 px or more in the mail: a confirmation or one-time
        /// code the reader copies. Painted a step larger, in a box that
        /// fits it.
        large: bool,
    },
    /// Cells are blocks, so a paragraph break, list marker, or quote inside a
    /// cell survives. Flattening a cell to inlines glues `<p>Hello</p><p>World</p>`
    /// into `HelloWorld` and drops those markers.
    Table {
        header: Vec<Vec<Block>>,
        rows: Vec<Vec<Vec<Block>>>,
        /// How each column's cells line up. Start unless the mail says
        /// otherwise with `align` or `text-align`.
        align: Vec<Align>,
    },
    Image {
        source: ImageSource,
        alt: String,
    },
    ThematicBreak,
    /// Fine print the sender set at 12 px or less (a newsletter footer).
    /// Painted small and dimmed.
    Small {
        blocks: Vec<Block>,
    },
    /// Outlook's `From:` / `Sent:` / `Subject:` block over the message it
    /// answers (`#divRplyFwdMsg`). Painted as its blocks; quoted history
    /// starts here.
    ReplyHeader {
        blocks: Vec<Block>,
    },
}

/// A table column's alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListLabelStyle {
    #[default]
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

impl ListLabelStyle {
    /// The label as it appears in the reader, including the period.
    pub fn label(self, value: u64) -> String {
        let mut label = match self {
            Self::LowerAlpha | Self::UpperAlpha if value > 0 => {
                let base = if self == Self::LowerAlpha { b'a' } else { b'A' };
                let mut value = value;
                let mut letters = Vec::new();
                while value > 0 {
                    value -= 1;
                    letters.push(char::from(base + (value % 26) as u8));
                    value /= 26;
                }
                letters.into_iter().rev().collect()
            }
            Self::LowerRoman | Self::UpperRoman if (1..=3999).contains(&value) => {
                let mut remaining = value;
                let mut label = String::new();
                for (number, letters) in ROMAN_DIGITS {
                    while remaining >= number {
                        label.push_str(letters);
                        remaining -= number;
                    }
                }
                if self == Self::UpperRoman {
                    label.make_ascii_uppercase();
                }
                label
            }
            _ => value.to_string(),
        };
        label.push('.');
        label
    }
}

pub(crate) const ROMAN_DIGITS: [(u64, &str); 13] = [
    (1000, "m"),
    (900, "cm"),
    (500, "d"),
    (400, "cd"),
    (100, "c"),
    (90, "xc"),
    (50, "l"),
    (40, "xl"),
    (10, "x"),
    (9, "ix"),
    (5, "v"),
    (4, "iv"),
    (1, "i"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    /// `<s>`, `<del>`, `<strike>`: a struck-through price, an edit.
    Strike(Vec<Inline>),
    Code(String),
    Link {
        href: String,
        children: Vec<Inline>,
    },
    Image {
        source: ImageSource,
        alt: String,
    },
    LineBreak,
}

/// Where an image's bytes come from. Remote URLs are never fetched here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    Remote(String),
    Cid(String),
    Data { media_type: String, bytes: Vec<u8> },
}

/// Decoded `data:` images larger than this are not stored. The painter keeps
/// the placeholder instead of the bytes. Matches the raster cap in the client.
pub const MAX_DATA_IMAGE_BYTES: usize = 32 * 1024 * 1024;

impl StyledDocument {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn plain_text(&self) -> String {
        Self::blocks_text(&self.blocks)
    }

    pub fn blocks_text(blocks: &[Block]) -> String {
        let mut out = String::new();
        for block in blocks {
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
        Block::List {
            ordered,
            start,
            style,
            items,
        } => {
            for (index, item) in items.iter().enumerate() {
                if *ordered {
                    out.push_str(&style.label(start.saturating_add(index as u64)));
                    out.push(' ');
                }
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
        Block::Code { text, .. } => {
            out.push_str(text);
            out.push('\n');
        }
        Block::Small { blocks } | Block::ReplyHeader { blocks } => push_blocks(out, blocks),
        Block::Table { header, rows, .. } => {
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
            | Inline::Strike(children)
            | Inline::Link { children, .. } => {
                push_inlines(out, children);
            }
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::LineBreak => out.push('\n'),
        }
    }
}
