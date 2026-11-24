//! Reconstruct lists whose markers and indents are part of the message text.

use std::{mem, ops::Range};

use crate::document::ROMAN_DIGITS;
use crate::{Block, Inline, ListItem, ListLabelStyle};

pub(crate) struct Marker<'a> {
    pub(crate) token: &'a str,
    pub(crate) gap: &'a str,
    pub(crate) content: &'a str,
    pub(crate) label: Label,
}

pub(crate) fn marker(text: &str) -> Option<Marker<'_>> {
    let text = text.trim_start();
    let token = text.split_whitespace().next()?;
    let label = parse_label(token)?;
    let rest = &text[token.len()..];
    let content = rest.trim_start();
    let gap = &rest[..rest.len() - content.len()];
    (!gap.is_empty()).then_some(Marker {
        token,
        gap,
        content,
        label,
    })
}

impl Marker<'_> {
    pub(crate) fn has_padding(&self) -> bool {
        self.gap.contains(['\u{a0}', '\t'])
            || (!self.gap.contains(['\r', '\n']) && self.gap.starts_with("  "))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Ordinal {
    style: ListLabelStyle,
    value: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct Label {
    ordinal: Option<Ordinal>,
    // A single Roman digit can also be an alphabetic label.
    roman: Option<u64>,
}

fn parse_label(token: &str) -> Option<Label> {
    // The private-use characters are Word's Symbol and Wingdings bullets.
    if matches!(
        token,
        "-" | "o" | "·" | "§" | "•" | "▪" | "\u{f0b7}" | "\u{f0a7}"
    ) {
        return Some(Label {
            ordinal: None,
            roman: None,
        });
    }
    let text = token.strip_suffix('.')?;
    let roman = roman_value(text);
    let (style, value) = match text.as_bytes() {
        [letter @ b'a'..=b'z'] => (ListLabelStyle::LowerAlpha, u64::from(letter - b'a' + 1)),
        [letter @ b'A'..=b'Z'] => (ListLabelStyle::UpperAlpha, u64::from(letter - b'A' + 1)),
        [b'1'..=b'9', ..] if text.len() <= 3 && text.bytes().all(|byte| byte.is_ascii_digit()) => {
            (ListLabelStyle::Decimal, text.parse().ok()?)
        }
        _ => (ListLabelStyle::LowerRoman, roman?),
    };
    Some(Label {
        ordinal: Some(Ordinal { style, value }),
        roman,
    })
}

fn roman_value(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 15 || !text.bytes().all(|byte| b"ivxlcdm".contains(&byte)) {
        return None;
    }
    let mut rest = text;
    let mut value = 0;
    for (number, digits) in ROMAN_DIGITS {
        while let Some(after) = rest.strip_prefix(digits) {
            rest = after;
            value += number;
        }
    }
    (rest.is_empty() && ListLabelStyle::LowerRoman.label(value).strip_suffix('.') == Some(text))
        .then_some(value)
}

#[derive(Clone, Copy)]
pub(crate) struct Indent {
    pub level: u32,
    /// Left margin in HTML, or the item's text column in a plain message.
    pub text: u32,
}

/// Delay committing an unindented first marker until a second item confirms
/// the run. Keep the source entries needed to replay a rejected run as prose.
pub(crate) struct ListRun<T> {
    levels: Vec<OpenList>,
    blocks: Vec<Block>,
    fallback: Vec<T>,
    confirmed: bool,
}

impl<T> Default for ListRun<T> {
    fn default() -> Self {
        Self {
            levels: Vec::new(),
            blocks: Vec::new(),
            fallback: Vec::new(),
            confirmed: false,
        }
    }
}

impl<T> ListRun<T> {
    pub(crate) fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    pub(crate) fn item(
        &mut self,
        indent: Indent,
        label: Label,
        explicit_indent: bool,
        blocks: Vec<Block>,
        original: T,
    ) {
        self.confirmed |= explicit_indent || !self.is_empty();
        self.keep_original(original);

        // Office uses at most nine levels. Cap inferred nesting as well so
        // arbitrary mail indent values cannot create an unbounded tree.
        const MAX_LEVELS: usize = 9;
        while self.levels.len() > 1
            && self
                .levels
                .last()
                .is_some_and(|level| indent.level < level.indent.level)
        {
            self.close_list();
        }
        let at_limit = self.levels.len() == MAX_LEVELS;
        let same_level = self
            .levels
            .last()
            .is_some_and(|level| indent.level <= level.indent.level || at_limit);
        let mut ordinal = label.ordinal;
        if same_level {
            let level = self.levels.last_mut().expect("a list level is open");
            ordinal = level.resolve_ordinal(label);
            if level.accepts(ordinal) {
                level.indent.level = level.indent.level.min(indent.level);
                level.indent.text = indent.text;
                level.items.push(ListItem { blocks });
                return;
            }
            self.close_list();
        }
        self.levels.push(OpenList {
            indent,
            start: ordinal,
            first_roman: label.roman,
            items: vec![ListItem { blocks }],
        });
    }

    pub(crate) fn continuation(
        &mut self,
        indent: u32,
        blocks: impl FnOnce() -> Vec<Block>,
        original: T,
    ) -> bool {
        let Some(level) = self
            .levels
            .iter()
            .rposition(|level| indent > 0 && indent >= level.indent.text)
        else {
            return false;
        };
        let blocks = blocks();
        while self.levels.len() > level + 1 {
            self.close_list();
        }
        self.levels[level].last_item().blocks.extend(blocks);
        self.keep_original(original);
        true
    }

    pub(crate) fn keep_original(&mut self, original: T) {
        if !self.confirmed {
            self.fallback.push(original);
        }
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Block>, Vec<T>> {
        let mut run = mem::take(self);
        if !run.confirmed {
            return Err(run.fallback);
        }
        while !run.levels.is_empty() {
            run.close_list();
        }
        Ok(run.blocks)
    }

    fn close_list(&mut self) {
        let list = self.levels.pop().expect("a list level is open");
        let target = self
            .levels
            .last_mut()
            .map_or(&mut self.blocks, |parent| &mut parent.last_item().blocks);
        let ordered = list.start.is_some();
        let start = list.start.map_or(1, |start| start.value);
        let style = list
            .start
            .map_or(ListLabelStyle::Decimal, |start| start.style);
        target.push(Block::List {
            ordered,
            start,
            style,
            items: list.items,
        });
    }
}

struct OpenList {
    indent: Indent,
    start: Option<Ordinal>,
    first_roman: Option<u64>,
    items: Vec<ListItem>,
}

impl OpenList {
    fn last_item(&mut self) -> &mut ListItem {
        self.items.last_mut().expect("an open list has an item")
    }

    fn resolve_ordinal(&mut self, label: Label) -> Option<Ordinal> {
        let Some(current) = self.start else {
            return label.ordinal;
        };
        let Some(roman) = label.roman else {
            return label.ordinal;
        };
        let style = ListLabelStyle::LowerRoman;
        if current.style == style {
            return Some(Ordinal {
                style,
                value: roman,
            });
        }
        // A following unambiguous Roman label resolves a first "i." or "v.".
        if self.items.len() == 1
            && label.ordinal.is_some_and(|ordinal| ordinal.style == style)
            && let Some(first) = self.first_roman.filter(|first| *first + 1 == roman)
        {
            self.start = Some(Ordinal {
                style,
                value: first,
            });
        }
        label.ordinal
    }

    fn accepts(&self, ordinal: Option<Ordinal>) -> bool {
        self.start.map(|start| Ordinal {
            value: start.value.saturating_add(self.items.len() as u64),
            ..start
        }) == ordinal
    }
}

fn advance_column(column: u32, ch: char) -> u32 {
    column.saturating_add(if ch == '\t' { 4 - column % 4 } else { 1 })
}

fn plain_indent(line: &str) -> u32 {
    line.chars()
        .take_while(|ch| matches!(ch, ' ' | '\t' | '\u{a0}'))
        .fold(0, advance_column)
}

fn plain_list_indent(level: u32, indent: u32, marker: &Marker<'_>) -> Indent {
    let after_marker = indent.saturating_add(marker.token.chars().count() as u32);
    Indent {
        level,
        text: marker.gap.chars().fold(after_marker, advance_column),
    }
}

fn plain_item(line: &str) -> Option<Marker<'_>> {
    let marker = marker(line)?;
    (!marker.content.is_empty() && marker.has_padding()).then_some(marker)
}

// Word's plain alternative can lose the leading indentation but keep the
// usual second- and third-level glyphs. Read them relative to the first
// item so a run of the same glyph always stays at one level.
fn word_bullet_level(token: &str) -> u32 {
    match token {
        "o" => 1,
        "▪" | "§" | "\u{f0a7}" => 2,
        _ => 0,
    }
}

fn plain_paragraph(text: &str) -> Vec<Block> {
    vec![Block::Paragraph {
        inlines: vec![Inline::Text(text.trim().to_string())],
    }]
}

/// Byte ranges of recognized typed lists and the blocks they replace. Prose
/// between the ranges can still be displayed with the caller's plain style.
pub fn plain_list_runs(body: &str) -> Vec<(Range<usize>, Vec<Block>)> {
    let mut lines = body
        .split_inclusive('\n')
        .scan(0, |at, line| {
            let start = *at;
            *at += line.len();
            Some((start, line))
        })
        .peekable();
    let mut lists = Vec::new();
    while let Some((start, line)) = lines.next() {
        let indent = plain_indent(line);
        let Some(marker) = plain_item(line) else {
            continue;
        };
        let first_indent = indent;
        let first_level = word_bullet_level(marker.token);
        let mut run = ListRun::default();
        run.item(
            plain_list_indent(indent, indent, &marker),
            marker.label,
            indent > 0,
            plain_paragraph(marker.content),
            (),
        );
        let mut end = start + line.len();
        while let Some(&(at, line)) = lines.peek() {
            let indent = plain_indent(line);
            if let Some(marker) = plain_item(line) {
                let level = if first_indent == 0 && indent == 0 {
                    word_bullet_level(marker.token).saturating_sub(first_level)
                } else {
                    indent
                };
                run.item(
                    plain_list_indent(level, indent, &marker),
                    marker.label,
                    indent > 0,
                    plain_paragraph(marker.content),
                    (),
                );
            } else if line.trim().is_empty()
                || !run.continuation(indent, || plain_paragraph(line), ())
            {
                break;
            }
            end = at + line.len();
            lines.next();
        }
        if let Ok(blocks) = run.finish() {
            lists.push((start..end, blocks));
        }
    }
    lists
}
