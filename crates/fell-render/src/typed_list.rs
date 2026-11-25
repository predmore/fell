//! Reconstruct lists whose markers and indents are part of the message text.

use std::{mem, ops::Range};

use crate::document::ROMAN_DIGITS;
use crate::{Block, ListItem, ListLabelStyle};

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
        let letters = self
            .label
            .ordinal
            .is_some_and(|ordinal| ordinal.style != ListLabelStyle::Decimal);
        let nbsp = if letters { 2 } else { 1 };
        let spaces = if letters { "   " } else { "  " };
        self.gap.matches('\u{a0}').take(nbsp).count() == nbsp
            || (!self.gap.contains(['\r', '\n'])
                && (self.gap.contains('\t') || self.gap.contains(spaces)))
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
    roman: Option<Ordinal>,
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
    let roman = roman_ordinal(text);
    let (style, value) = match text.as_bytes() {
        [letter @ b'a'..=b'z'] => (ListLabelStyle::LowerAlpha, u64::from(letter - b'a' + 1)),
        [letter @ b'A'..=b'Z'] => (ListLabelStyle::UpperAlpha, u64::from(letter - b'A' + 1)),
        [b'1'..=b'9', ..] if text.len() <= 3 && text.bytes().all(|byte| byte.is_ascii_digit()) => {
            (ListLabelStyle::Decimal, text.parse().ok()?)
        }
        _ => {
            let ordinal = roman?;
            (ordinal.style, ordinal.value)
        }
    };
    Some(Label {
        ordinal: Some(Ordinal { style, value }),
        roman,
    })
}

fn roman_ordinal(text: &str) -> Option<Ordinal> {
    if text.is_empty() || text.len() > 15 {
        return None;
    }
    let style = if text.bytes().all(|byte| b"ivxlcdm".contains(&byte)) {
        ListLabelStyle::LowerRoman
    } else if text.bytes().all(|byte| b"IVXLCDM".contains(&byte)) {
        ListLabelStyle::UpperRoman
    } else {
        return None;
    };
    let lower = text.to_ascii_lowercase();
    let mut rest = lower.as_str();
    let mut value = 0;
    for (number, digits) in ROMAN_DIGITS {
        while let Some(after) = rest.strip_prefix(digits) {
            rest = after;
            value += number;
        }
    }
    (rest.is_empty() && style.label(value).strip_suffix('.') == Some(text))
        .then_some(Ordinal { style, value })
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

    /// An incompatible root marker starts a new candidate run. A previously
    /// rejected singleton then falls back to its original source.
    pub(crate) fn starts_new_root(&mut self, indent: Indent, label: Label) -> bool {
        self.levels.first_mut().is_some_and(|root| {
            if indent.level > root.indent.level {
                return false;
            }
            let ordinal = root.resolve_ordinal(label);
            !root.accepts(ordinal)
        })
    }

    pub(crate) fn item(
        &mut self,
        indent: Indent,
        label: Label,
        explicit_indent: bool,
        blocks: Vec<Block>,
        original: T,
    ) {
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
                self.confirmed = true;
                self.fallback.clear();
                level.indent.level = level.indent.level.min(indent.level);
                level.indent.text = indent.text;
                level.items.push(ListItem { blocks });
                return;
            }
            self.close_list();
        }
        self.confirmed |= explicit_indent;
        if self.confirmed {
            self.fallback.clear();
        } else {
            self.keep_original(original);
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
    first_roman: Option<Ordinal>,
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
        if current.style == roman.style {
            return Some(roman);
        }
        // A following unambiguous Roman label resolves a first "i." or "v.".
        if self.items.len() == 1
            && label.ordinal == Some(roman)
            && let Some(first) = self
                .first_roman
                .filter(|first| first.style == roman.style && first.value + 1 == roman.value)
        {
            self.start = Some(first);
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
        inlines: crate::plain_links::plain_link_inlines(text.trim()),
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
    let mut lists: Vec<(Range<usize>, Vec<Block>)> = Vec::new();
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
                let item_indent = plain_list_indent(level, indent, &marker);
                if run.starts_new_root(item_indent, marker.label) {
                    break;
                }
                run.item(
                    item_indent,
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
            if let Some((range, previous)) = lists.last_mut()
                && range.end == start
            {
                range.end = end;
                previous.extend(blocks);
            } else {
                lists.push((start..end, blocks));
            }
        }
    }
    lists
}
