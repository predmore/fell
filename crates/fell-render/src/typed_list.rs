//! Reconstruct lists whose markers and indents are part of the message text.

use std::{mem, ops::Range};

use crate::{Block, Inline, ListItem};

pub(crate) struct Marker<'a> {
    pub(crate) token: &'a str,
    pub(crate) gap: &'a str,
    pub(crate) content: &'a str,
    pub(crate) ordered: bool,
}

pub(crate) fn marker(text: &str) -> Option<Marker<'_>> {
    let text = text.trim_start();
    let token = text.split_whitespace().next()?;
    let ordered = token.strip_suffix('.').is_some_and(|label| {
        (label.len() == 1 && label.as_bytes()[0].is_ascii_alphabetic())
            || (!label.is_empty() && label.bytes().all(|byte| byte.is_ascii_digit()))
    });
    // The private-use characters are Word's Symbol and Wingdings bullets.
    if !ordered
        && !matches!(
            token,
            "-" | "o" | "·" | "§" | "•" | "▪" | "\u{f0b7}" | "\u{f0a7}"
        )
    {
        return None;
    }
    let rest = &text[token.len()..];
    let content = rest.trim_start();
    let gap = &rest[..rest.len() - content.len()];
    (!gap.is_empty()).then_some(Marker {
        token,
        gap,
        content,
        ordered,
    })
}

impl Marker<'_> {
    pub(crate) fn has_padding(&self) -> bool {
        self.gap.contains(['\u{a0}', '\t'])
            || (!self.gap.contains(['\r', '\n']) && self.gap.starts_with("  "))
    }
}

struct Entry {
    indent: u32,
    ordered: Option<bool>,
    blocks: Vec<Block>,
}

/// Delay committing an unindented first marker until a second item confirms
/// the run. Keep the source entries needed to replay a rejected run as prose.
pub(crate) struct ListRun<T> {
    entries: Vec<Entry>,
    fallback: Vec<T>,
    confirmed: bool,
    last_indent: u32,
}

impl<T> Default for ListRun<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            fallback: Vec::new(),
            confirmed: false,
            last_indent: 0,
        }
    }
}

impl<T> ListRun<T> {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn item(
        &mut self,
        indent: u32,
        ordered: bool,
        explicit_indent: bool,
        blocks: Vec<Block>,
        original: T,
    ) {
        self.confirmed |= explicit_indent || !self.is_empty();
        self.last_indent = indent;
        self.push(indent, Some(ordered), blocks, original);
    }

    pub(crate) fn accepts_continuation(&self, indent: u32) -> bool {
        !self.is_empty() && indent > 0 && indent >= self.last_indent
    }

    pub(crate) fn continuation(&mut self, blocks: Vec<Block>, original: T) {
        self.push(self.last_indent, None, blocks, original);
    }

    fn push(&mut self, indent: u32, ordered: Option<bool>, blocks: Vec<Block>, original: T) {
        self.entries.push(Entry {
            indent,
            ordered,
            blocks,
        });
        if !self.confirmed {
            self.fallback.push(original);
        }
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Block>, Vec<T>> {
        let run = mem::take(self);
        if !run.confirmed {
            return Err(run.fallback);
        }

        // Office uses at most nine levels. Cap inferred nesting as well so
        // arbitrary mail indent values cannot create an unbounded tree.
        const MAX_LEVELS: usize = 9;
        let mut out = Vec::new();
        let mut levels: Vec<OpenList> = Vec::new();
        for entry in run.entries {
            let Some(ordered) = entry.ordered else {
                if let Some(item) = levels.last_mut().and_then(|level| level.items.last_mut()) {
                    item.blocks.extend(entry.blocks);
                }
                continue;
            };
            while levels.len() > 1
                && levels
                    .last()
                    .is_some_and(|level| entry.indent < level.indent)
            {
                close_list(&mut levels, &mut out);
            }
            if let Some(level) = levels.last()
                && (entry.indent <= level.indent || levels.len() == MAX_LEVELS)
                && ordered != level.ordered
            {
                close_list(&mut levels, &mut out);
            }
            if levels
                .last()
                .is_none_or(|level| entry.indent > level.indent)
                && levels.len() < MAX_LEVELS
            {
                levels.push(OpenList {
                    indent: entry.indent,
                    ordered,
                    items: Vec::new(),
                });
            }
            let level = levels.last_mut().expect("a list level is open");
            level.indent = level.indent.min(entry.indent);
            level.items.push(ListItem {
                blocks: entry.blocks,
            });
        }
        while !levels.is_empty() {
            close_list(&mut levels, &mut out);
        }
        Ok(out)
    }
}

struct OpenList {
    indent: u32,
    ordered: bool,
    items: Vec<ListItem>,
}

fn close_list(levels: &mut Vec<OpenList>, out: &mut Vec<Block>) {
    let OpenList { ordered, items, .. } = levels.pop().expect("a list level is open");
    let target = levels
        .last_mut()
        .and_then(|parent| parent.items.last_mut())
        .map_or(out, |item| &mut item.blocks);
    target.push(Block::List { ordered, items });
}

fn plain_indent(line: &str) -> u32 {
    line.chars()
        .take_while(|ch| matches!(ch, ' ' | '\t' | '\u{a0}'))
        .fold(0u32, |indent, ch| {
            indent.saturating_add(if ch == '\t' { 4 - indent % 4 } else { 1 })
        })
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
            indent,
            marker.ordered,
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
                    level,
                    marker.ordered,
                    indent > 0,
                    plain_paragraph(marker.content),
                    (),
                );
            } else if !line.trim().is_empty() && run.accepts_continuation(indent) {
                run.continuation(plain_paragraph(line), ());
            } else {
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
