//! Quoted history and signatures in received mail.
//!
//! The conversation view folds earlier messages and signatures away, as it
//! already does for `>` lines and `-- ` in plain text. [`fold_history`]
//! marks the top-level blocks of a [`StyledDocument`] that are history.
//!
//! The HTML walk helps by turning the containers mail clients write around
//! quoted history (`gmail_quote`, `protonmail_quote`, `yahoo_quoted`) into one
//! [`Block::Quote`], and by starting a signature container (`gmail_signature`,
//! `moz-signature`, Outlook's `#Signature`) with the RFC 3676 `-- ` line.
//! Outlook quotes without either: it writes a `From:` / `Sent:` /
//! `Subject:` header block (in `#divRplyFwdMsg`, or after
//! `-----Original Message-----`), and everything after it is the earlier
//! mail.

use std::ops::Range;

use crate::document::{Block, Inline, StyledDocument};

/// What a folded section holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fold {
    Quote,
    Signature,
    /// A signature with the quoted history right under it: one fold.
    SignatureAndQuote,
    /// Quoted history with the sender's signature under it.
    QuoteAndSignature,
}

/// A run of top-level blocks. `fold` is `None` for what the sender wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub fold: Option<Fold>,
    pub blocks: Range<usize>,
    /// A short signature shown in place ("Sent from my iPhone"): painted
    /// in the dimmed colour. Only on an open section.
    pub quiet: bool,
    /// Quoted text the sender answers below it, in an interleaved reply:
    /// open, under the history rule, in the dimmed colour. Only on an open
    /// section.
    pub quoted: bool,
}

/// A document and its sections, in order. The sections cover every block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FoldedDocument {
    pub document: StyledDocument,
    pub sections: Vec<Section>,
}

impl FoldedDocument {
    /// The whole document as one unfolded section.
    pub fn unfolded(document: StyledDocument) -> Self {
        let len = document.blocks.len();
        let sections = if len == 0 {
            Vec::new()
        } else {
            vec![Section {
                fold: None,
                blocks: 0..len,
                quiet: false,
                quoted: false,
            }]
        };
        Self { document, sections }
    }

    pub fn blocks(&self, section: &Section) -> &[Block] {
        &self.document.blocks[section.blocks.clone()]
    }

    pub fn has_folds(&self) -> bool {
        self.sections.iter().any(|section| section.fold.is_some())
    }
}

pub(crate) enum Marker {
    Quote,
    Signature,
    /// Outlook's header over the message it answers.
    ReplyHeader,
}

/// History containers that mail clients mark with a class or id.
pub(crate) fn marker(element: &scraper::node::Element) -> Option<Marker> {
    const QUOTES: &[&str] = &["gmail_quote", "gmail_quote_container", "protonmail_quote"];
    const SIGNATURES: &[&str] = &[
        "gmail_signature",
        "moz-signature",
        "protonmail_signature_block",
    ];
    // Yahoo prefixes its classes per message: `ydp5a6b7c8dyahoo_quoted`.
    if element.classes().any(|class| {
        QUOTES.iter().any(|known| class.eq_ignore_ascii_case(known))
            || class.to_ascii_lowercase().ends_with("yahoo_quoted")
    }) {
        return Some(Marker::Quote);
    }
    if element
        .id()
        .is_some_and(|id| id.eq_ignore_ascii_case("divRplyFwdMsg"))
    {
        return Some(Marker::ReplyHeader);
    }
    if element.classes().any(|class| {
        SIGNATURES
            .iter()
            .any(|known| class.eq_ignore_ascii_case(known))
    }) || element
        .id()
        .is_some_and(|id| id.eq_ignore_ascii_case("signature"))
    {
        return Some(Marker::Signature);
    }
    None
}

/// One quote for a history container. The `blockquote` inside it is the same
/// quote, so its blocks join the attribution line under one rule. A reply
/// written inline (Gmail puts the answers between the quoted pieces, inside
/// the container) stays as it is, so each answer keeps its own place.
pub(crate) fn quote_container(blocks: Vec<Block>) -> Vec<Block> {
    if blocks.is_empty() {
        return blocks;
    }
    let first_quote = blocks
        .iter()
        .position(|block| matches!(block, Block::Quote { .. }));
    if let Some(first) = first_quote
        && blocks[first + 1..]
            .iter()
            .any(|block| !matches!(block, Block::Quote { .. }) && !is_blank(block))
    {
        return blocks;
    }
    let mut joined = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::Quote { blocks } => joined.extend(blocks),
            other => joined.push(other),
        }
    }
    vec![Block::Quote { blocks: joined }]
}

/// A signature container starts with the `-- ` line plain text uses.
pub(crate) fn signature_container(mut blocks: Vec<Block>) -> Vec<Block> {
    if blocks.is_empty() {
        return blocks;
    }
    let marked = blocks.first().is_some_and(|block| match block {
        Block::Paragraph { inlines } => first_line_is_rule(inlines),
        _ => false,
    });
    if !marked {
        blocks.insert(
            0,
            Block::Paragraph {
                inlines: vec![Inline::Text(crate::html::SIGNATURE_DELIMITER.into())],
            },
        );
    }
    blocks
}

/// Mark quoted history and signatures among the top-level blocks.
///
/// Only the history after the sender's last own words folds: in an
/// interleaved reply (a question quoted, answered, the next quoted) the
/// quotes the sender answers stay open, marked [`Section::quoted`].
/// `subject` is the message's own subject: a forward (`Fwd:`, `FW:` and
/// their translations) is what the sender is sending, so nothing in it
/// folds as quoted history, and neither does anything under Gmail's or
/// Apple Mail's forward line.
///
/// Folds nothing when that would leave nothing unfolded, so a bare forward
/// or a message that is all quote still shows.
pub fn fold_history(document: StyledDocument, subject: &str) -> FoldedDocument {
    let (blocks, header_ends) =
        flatten_reply_headers(split_history_lines(split_signature_rules(document.blocks)));
    let mut kinds: Vec<Option<Fold>> = Vec::with_capacity(blocks.len());
    let mut drop = vec![false; blocks.len()];
    let mut rest_is_quote = false;
    let mut in_signature = false;
    // Under a forward line, or in a forward, quotes are the forwarded mail.
    let mut forwarding = is_forward_subject(subject);
    for (at, block) in blocks.iter().enumerate() {
        if rest_is_quote {
            kinds.push(Some(Fold::Quote));
            continue;
        }
        let header = match header_ends[at] {
            Some(end) => {
                content_after(&blocks[end..]).then_some(is_forward_header(&blocks[at..end]))
            }
            None => starts_outlook_history(&blocks, at),
        };
        match header {
            // The forwarded mail's own header: it ends the sender's
            // signature and stays open, with everything under it.
            Some(true) => {
                in_signature = false;
                forwarding = true;
                kinds.push(None);
                continue;
            }
            Some(false) if forwarding => {
                in_signature = false;
                kinds.push(None);
                continue;
            }
            Some(false) => {
                rest_is_quote = true;
                // The rule Outlook draws above the header goes with it.
                if at > 0 && matches!(blocks[at - 1], Block::ThematicBreak) {
                    kinds[at - 1] = Some(Fold::Quote);
                }
                kinds.push(Some(Fold::Quote));
                continue;
            }
            None => {}
        }
        match block {
            // A forward is what the sender is sending, even under their
            // signature: it ends the signature and stays open.
            Block::Quote { blocks: inner } if forwarding || is_forward(inner) => {
                in_signature = false;
                kinds.push(None);
            }
            Block::Paragraph { inlines } if is_forward_line(&inlines_text(inlines)) => {
                in_signature = false;
                forwarding = true;
                kinds.push(None);
            }
            Block::Quote { .. } => {
                in_signature = false;
                // "On Monday, Ada wrote:" belongs to the quote under it.
                if at > 0 && kinds[at - 1].is_none() && is_attribution(&blocks[at - 1]) {
                    kinds[at - 1] = Some(Fold::Quote);
                }
                kinds.push(Some(Fold::Quote));
            }
            Block::Paragraph { inlines } if first_line_is_rule(inlines) => {
                in_signature = true;
                drop[at] = inlines_text(inlines).trim() == "--";
                kinds.push(Some(Fold::Signature));
            }
            Block::Paragraph { inlines } if is_mobile_signature(inlines) => {
                kinds.push(Some(Fold::Signature));
            }
            Block::Code { text, .. } if code_starts_with_rule(text) => {
                in_signature = true;
                kinds.push(Some(Fold::Signature));
            }
            _ if in_signature => kinds.push(Some(Fold::Signature)),
            _ => kinds.push(None),
        }
    }
    let own = |at: usize| {
        kinds[at].is_none()
            && !drop[at]
            && !is_blank(&blocks[at])
            // A disclaimer a server adds under the quote is not an answer.
            && !matches!(blocks[at], Block::Small { .. })
    };
    let Some(last_own) = (0..blocks.len()).rev().find(|&at| own(at)) else {
        return FoldedDocument::unfolded(StyledDocument { blocks });
    };
    // Quotes the sender answers under them stay open.
    let mut quoted = vec![false; blocks.len()];
    for at in 0..last_own {
        if kinds[at] == Some(Fold::Quote) {
            kinds[at] = None;
            quoted[at] = true;
        }
    }
    let mut kept = Vec::with_capacity(blocks.len());
    let mut sections: Vec<Section> = Vec::new();
    for (((block, kind), dropped), quoted) in blocks.into_iter().zip(kinds).zip(drop).zip(quoted) {
        if dropped {
            continue;
        }
        let block = match (kind, block) {
            (Some(Fold::Signature), Block::Paragraph { inlines }) => {
                match strip_first_rule_line(inlines) {
                    Some(inlines) => Block::Paragraph { inlines },
                    None => continue,
                }
            }
            (Some(Fold::Signature), Block::Code { text, large })
                if code_starts_with_rule(&text) =>
            {
                let rest = text
                    .split_once('\n')
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_default();
                if rest.trim().is_empty() {
                    continue;
                }
                Block::Code { text: rest, large }
            }
            (_, block) => block,
        };
        let at = kept.len();
        kept.push(block);
        match sections.last_mut() {
            Some(last) if last.fold == kind && last.quoted == quoted => last.blocks.end = at + 1,
            _ => sections.push(Section {
                fold: kind,
                blocks: at..at + 1,
                quiet: false,
                quoted,
            }),
        }
    }
    let sections = join_adjacent_folds(show_short_signatures(sections, &kept));
    FoldedDocument {
        document: StyledDocument { blocks: kept },
        sections,
    }
}

/// Longest signature shown in place. A name and a line under it cost less
/// to read than the fold that would hide them.
const INLINE_SIGNATURE_LINES: usize = 2;

/// Show signatures of [`INLINE_SIGNATURE_LINES`] or fewer in place, as a
/// quiet section the painter dims.
fn show_short_signatures(sections: Vec<Section>, blocks: &[Block]) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::with_capacity(sections.len());
    for mut section in sections {
        if section.fold == Some(Fold::Signature) {
            let text = StyledDocument {
                blocks: blocks[section.blocks.clone()].to_vec(),
            }
            .plain_text();
            let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
            if lines <= INLINE_SIGNATURE_LINES {
                section.fold = None;
                section.quiet = true;
            }
        }
        match out.last_mut() {
            Some(last)
                if last.fold == section.fold
                    && last.quiet == section.quiet
                    && last.quoted == section.quoted =>
            {
                last.blocks.end = section.blocks.end
            }
            _ => out.push(section),
        }
    }
    out
}

/// A signature with the quote right under it (or over it) is one fold:
/// `Signature and quoted text`. One line to open, not two.
fn join_adjacent_folds(sections: Vec<Section>) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::with_capacity(sections.len());
    for section in sections {
        if let (Some(last), Some(next)) = (out.last_mut(), section.fold)
            && let Some(above) = last.fold
        {
            let kind = match (above, next) {
                (Fold::Signature, Fold::Quote) => Some(Fold::SignatureAndQuote),
                (Fold::Quote, Fold::Signature) => Some(Fold::QuoteAndSignature),
                (Fold::SignatureAndQuote, Fold::Quote | Fold::Signature) => {
                    Some(Fold::SignatureAndQuote)
                }
                (Fold::QuoteAndSignature, Fold::Quote | Fold::Signature) => {
                    Some(Fold::QuoteAndSignature)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                last.fold = Some(kind);
                last.blocks.end = section.blocks.end;
                continue;
            }
        }
        out.push(section);
    }
    out
}

/// Give a history line its own block when it ends or starts a paragraph of
/// lines: an `On … wrote:` line over a quote, an Outlook `From:` header
/// line or `-----Original Message-----`, a forward line, and a phone's
/// `Sent from my …`.
fn split_history_lines(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    let mut blocks = blocks.into_iter().peekable();
    while let Some(block) = blocks.next() {
        let Block::Paragraph { inlines } = block else {
            out.push(block);
            continue;
        };
        let lines = split_lines(&inlines);
        if lines.len() < 2 {
            out.push(Block::Paragraph { inlines });
            continue;
        }
        let quote_next = matches!(blocks.peek(), Some(Block::Quote { .. }));
        let last_text = inlines_text(lines[lines.len() - 1]);
        let last_line_is_history = (quote_next && last_text.trim_end().ends_with("wrote:"))
            || is_phone_signature(&last_text)
            || is_forward_line(&last_text);
        let cut = if last_line_is_history {
            Some(lines.len() - 1)
        } else {
            lines
                .iter()
                .skip(1)
                .position(|line| {
                    let text = inlines_text(line);
                    let text = text.trim_start();
                    text.starts_with("-----Original Message-----")
                        || is_quoted_from_line(text)
                        || is_forward_line(text)
                })
                .map(|at| at + 1)
        };
        match cut {
            Some(at) => {
                out.push(Block::Paragraph {
                    inlines: join_lines(&lines[..at]),
                });
                out.push(Block::Paragraph {
                    inlines: join_lines(&lines[at..]),
                });
            }
            None => out.push(Block::Paragraph { inlines }),
        }
    }
    out
}

fn split_lines(inlines: &[Inline]) -> Vec<&[Inline]> {
    inlines
        .split(|inline| matches!(inline, Inline::LineBreak))
        .collect()
}

fn join_lines(lines: &[&[Inline]]) -> Vec<Inline> {
    let mut out = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if at > 0 {
            out.push(Inline::LineBreak);
        }
        out.extend_from_slice(line);
    }
    out
}

/// Split a paragraph at a `--` line in its middle, so the signature starts
/// its own paragraph: `Thanks,<br>Jon<br>-- <br>Jon Doe` is two blocks.
fn split_signature_rules(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        let Block::Paragraph { inlines } = block else {
            out.push(block);
            continue;
        };
        match rule_line_start(&inlines) {
            Some(start) if start > 0 => {
                let mut before = inlines;
                let after = before.split_off(start);
                // The line break before the rule ends the first paragraph.
                while matches!(before.last(), Some(Inline::LineBreak)) {
                    before.pop();
                }
                if !before.is_empty() {
                    out.push(Block::Paragraph { inlines: before });
                }
                out.push(Block::Paragraph { inlines: after });
            }
            _ => out.push(Block::Paragraph { inlines }),
        }
    }
    out
}

/// Outlook's `#divRplyFwdMsg` header, painted as its own blocks. Returns
/// the blocks with each header flattened in place, and for each block where
/// a header starts, the index just past it.
fn flatten_reply_headers(blocks: Vec<Block>) -> (Vec<Block>, Vec<Option<usize>>) {
    let mut out = Vec::with_capacity(blocks.len());
    let mut ends = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::ReplyHeader { blocks: inner } if !inner.is_empty() => {
                let start = out.len();
                let end = start + inner.len();
                out.extend(inner);
                ends.push(Some(end));
                ends.extend(std::iter::repeat_n(None, end - start - 1));
            }
            Block::ReplyHeader { .. } => {}
            other => {
                out.push(other);
                ends.push(None);
            }
        }
    }
    (out, ends)
}

/// Whether anything a reader could see follows: a header over nothing is
/// the sender's own text, not history.
fn content_after(blocks: &[Block]) -> bool {
    blocks.iter().any(|block| !is_blank(block))
}

/// Index of the first inline of the first line that is the `-- ` rule.
fn rule_line_start(inlines: &[Inline]) -> Option<usize> {
    let mut start = 0;
    for (at, inline) in inlines.iter().enumerate() {
        if matches!(inline, Inline::LineBreak) {
            if is_rule(&inlines[start..at]) {
                return Some(start);
            }
            start = at + 1;
        }
    }
    is_rule(&inlines[start..]).then_some(start)
}

/// RFC 3676: two dashes and a space, then nothing. A bare `--` is the
/// sender's own text (a divider, an em dash typed as two).
fn is_rule(line: &[Inline]) -> bool {
    !line.is_empty()
        && line.iter().all(|inline| matches!(inline, Inline::Text(_)))
        && is_rule_text(&inlines_text(line))
}

fn is_rule_text(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("-- ")
        .is_some_and(|rest| rest.trim().is_empty())
}

fn first_line_is_rule(inlines: &[Inline]) -> bool {
    rule_line_start(inlines) == Some(0)
}

/// Drop the `--` line. `None` when nothing is left.
fn strip_first_rule_line(inlines: Vec<Inline>) -> Option<Vec<Inline>> {
    if !first_line_is_rule(&inlines) {
        return Some(inlines);
    }
    let rest: Vec<Inline> = match inlines
        .iter()
        .position(|inline| matches!(inline, Inline::LineBreak))
    {
        Some(at) => inlines.into_iter().skip(at + 1).collect(),
        None => Vec::new(),
    };
    let blank = rest.iter().all(|inline| match inline {
        Inline::Text(text) => text.trim().is_empty(),
        Inline::LineBreak => true,
        _ => false,
    });
    (!blank).then_some(rest)
}

fn code_starts_with_rule(text: &str) -> bool {
    text.lines().next().is_some_and(is_rule_text)
}

/// "Sent from my iPhone", "Get Outlook for Android" and the like, which
/// phones and apps add under a message.
fn is_mobile_signature(inlines: &[Inline]) -> bool {
    is_phone_signature(&inlines_text(inlines))
}

fn is_phone_signature(text: &str) -> bool {
    let text = text.trim();
    text.len() < 60
        && !text.contains('\n')
        && ["Get Outlook for ", "Sent from my ", "Sent from Mail for "]
            .iter()
            .any(|start| text.starts_with(start))
}

/// Outlook's header over the earlier message, written as text: the older
/// `-----Original Message-----` line, or a `From:` line that names an
/// address followed by `Sent:` or `Date:` and `Subject:`, in this block or
/// the few after it. `Some(true)` when the header is a forward's
/// (`Subject: FW: …`), which stays open. A sender's own `From:` /
/// `Subject:` lines (a template, a ticket) name no address, and a header
/// with nothing under it quotes nothing; neither is a header.
fn starts_outlook_history(blocks: &[Block], at: usize) -> Option<bool> {
    let first = block_text(&blocks[at])?;
    let first = first.trim_start();
    if let Some(rest) = first.strip_prefix("-----Original Message-----") {
        return (!rest.trim().is_empty() || content_after(&blocks[at + 1..])).then_some(false);
    }
    if !first.lines().next().is_some_and(is_quoted_from_line) {
        return None;
    }
    let mut sent = false;
    for (offset, block) in blocks[at..].iter().take(5).enumerate() {
        let text = block_text(block)?;
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let line = line.trim_start();
            sent |= line.starts_with("Sent:") || line.starts_with("Date:");
            if let Some(subject) = line.strip_prefix("Subject:") {
                if !sent {
                    continue;
                }
                let below = lines[index + 1..]
                    .iter()
                    .any(|line| !line.trim().is_empty());
                return (below || content_after(&blocks[at + offset + 1..]))
                    .then(|| is_forward_subject(subject));
            }
        }
    }
    None
}

/// A `From:` line of a quoted header: it names an address, as every
/// client's reply header does (`From: Ada <ada@example.test>`,
/// `From: ada@example.test`, `From: Ada [mailto:ada@example.test]`).
pub fn is_quoted_from_line(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("From:")
        .is_some_and(names_an_address)
}

fn names_an_address(text: &str) -> bool {
    text.split(|ch: char| ch.is_whitespace() || "<>[]():;,\"'".contains(ch))
        .any(|token| {
            token.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty()
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
            })
        })
}

/// A forward's subject: `Fwd:`, `FW:`, and the German, French and Spanish
/// `WG:`, `TR:` and `RV:`, after any `[list]` tags.
pub fn is_forward_subject(subject: &str) -> bool {
    let mut rest = subject.trim_start();
    while let Some(tagged) = rest.strip_prefix('[') {
        match tagged.split_once(']') {
            Some((_, after)) => rest = after.trim_start(),
            None => break,
        }
    }
    let lower = rest.to_lowercase();
    ["fwd:", "fw:", "wg:", "tr:", "rv:"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// An Outlook `#divRplyFwdMsg` header over a forward.
fn is_forward_header(blocks: &[Block]) -> bool {
    blocks.iter().filter_map(block_text).any(|text| {
        text.lines().any(|line| {
            line.trim_start()
                .strip_prefix("Subject:")
                .is_some_and(is_forward_subject)
        })
    })
}

/// Gmail's `---------- Forwarded message ---------` and Apple Mail's
/// `Begin forwarded message:`, written as a paragraph of their own.
fn is_forward_line(text: &str) -> bool {
    let text = text.trim_start();
    (text.starts_with('-') && text.contains("Forwarded message"))
        || text.starts_with("Begin forwarded message")
}

/// "On Mon, 5 Jan 2026 at 09:00, Ada <ada@example.test> wrote:"
fn is_attribution(block: &Block) -> bool {
    let Some(text) = block_text(block) else {
        return false;
    };
    let text = text.trim();
    text.len() < 400 && text.ends_with("wrote:")
}

fn is_forward(blocks: &[Block]) -> bool {
    let Some(text) = blocks.first().and_then(block_text) else {
        return false;
    };
    let text = text.trim_start();
    text.contains("Forwarded message") || text.starts_with("Begin forwarded message")
}

fn is_blank(block: &Block) -> bool {
    match block {
        Block::Paragraph { inlines } => inlines_text(inlines).trim().is_empty(),
        Block::Small { blocks } | Block::ReplyHeader { blocks } => blocks.iter().all(is_blank),
        Block::ThematicBreak => true,
        _ => false,
    }
}

fn block_text(block: &Block) -> Option<String> {
    match block {
        Block::Paragraph { inlines } | Block::Heading { inlines, .. } => {
            Some(inlines_text(inlines))
        }
        _ => None,
    }
}

fn inlines_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    push_inlines(&mut out, inlines);
    out
}

fn push_inlines(out: &mut String, inlines: &[Inline]) {
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) => out.push_str(text),
            Inline::Emphasis(children)
            | Inline::Strong(children)
            | Inline::Strike(children)
            | Inline::Link { children, .. } => push_inlines(out, children),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::LineBreak => out.push('\n'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html_to_document;

    fn folded(html: &str) -> FoldedDocument {
        fold_history(html_to_document(html), "Re: Plans")
    }

    fn texts(doc: &FoldedDocument) -> Vec<(Option<Fold>, String)> {
        doc.sections
            .iter()
            .map(|section| {
                let text = StyledDocument {
                    blocks: doc.blocks(section).to_vec(),
                }
                .plain_text();
                (section.fold, text)
            })
            .collect()
    }

    #[test]
    fn a_gmail_reply_folds_its_quote_and_signature() {
        let doc = folded(
            r#"<div dir="ltr"><div>Thursday works.</div><br clear="all"><div><br></div>-- <br><div dir="ltr" class="gmail_signature">Jon Ames<br>Fell<br>fell.test</div></div><br><div class="gmail_quote gmail_quote_container"><div dir="ltr" class="gmail_attr">On Mon, Jan 5, 2026 at 9:00 AM Ada &lt;ada@example.test&gt; wrote:<br></div><blockquote class="gmail_quote" style="margin:0px 0px 0px 0.8ex"><div dir="ltr">Does Thursday work?</div></blockquote></div>"#,
        );
        let sections = texts(&doc);
        // The signature and the quote under it are one fold.
        assert_eq!(sections.len(), 2, "{sections:?}");
        assert_eq!(sections[0], (None, "Thursday works.".into()));
        assert_eq!(sections[1].0, Some(Fold::SignatureAndQuote));
        assert!(sections[1].1.starts_with("Jon Ames\nFell\nfell.test\n"));
        assert!(sections[1].1.contains("wrote:"));
        assert!(sections[1].1.contains("Does Thursday work?"));
        // One quote rule for the container and its blockquote.
        let quote = doc.blocks(&doc.sections[1]);
        let Some(Block::Quote { blocks }) = quote.last() else {
            panic!("{quote:?}");
        };
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, Block::Quote { .. }))
        );
    }

    #[test]
    fn an_outlook_reply_chain_folds_from_the_first_header() {
        let doc = folded(
            "<div>Amen</div><div>Get Outlook for Android</div><div id=\"appendonsend\"></div><hr><div id=\"divRplyFwdMsg\"><font><b>From:</b> Ada &lt;ada@example.test&gt;<br><b>Sent:</b> Monday, January 5, 2026 9:00 AM<br><b>To:</b> Jon<br><b>Subject:</b> Re: Plans</font></div><div>Earlier text</div><hr><div><b>From:</b> Jon<br><b>Sent:</b> Sunday<br><b>Subject:</b> Plans</div><div>Even earlier</div>",
        );
        let sections = texts(&doc);
        // One line of signature shows in place, dimmed.
        assert_eq!(sections[0], (None, "Amen".into()));
        assert_eq!(sections[1], (None, "Get Outlook for Android".into()));
        assert!(doc.sections[1].quiet);
        assert_eq!(sections[2].0, Some(Fold::Quote));
        assert!(sections[2].1.contains("Earlier text"));
        assert!(sections[2].1.contains("Even earlier"));
        assert_eq!(sections.len(), 3);
    }

    #[test]
    fn a_from_line_without_the_rest_of_a_header_is_body() {
        let doc = folded("<p>From: the team, with thanks.</p><p>See you.</p>");
        assert!(!doc.has_folds());
    }

    #[test]
    fn a_signature_rule_in_a_paragraph_splits_it() {
        let doc = folded("<p>Thanks,<br>Jon<br>-- <br>Jon Ames<br>Fell<br>fell.test</p>");
        let sections = texts(&doc);
        assert_eq!(
            sections,
            vec![
                (None, "Thanks,\nJon".into()),
                (Some(Fold::Signature), "Jon Ames\nFell\nfell.test".into()),
            ]
        );
    }

    #[test]
    fn a_short_signature_shows_in_place() {
        let doc = folded("<p>Thanks,<br>Jon<br>-- <br>Jon Ames | Fell</p>");
        assert!(!doc.has_folds(), "{:?}", texts(&doc));
        // The delimiter itself is not painted.
        assert_eq!(doc.document.plain_text(), "Thanks,\nJon\nJon Ames | Fell");
    }

    #[test]
    fn two_dashes_without_the_space_are_not_a_signature() {
        for html in [
            "<p>Before</p><div>--</div><p>One</p><p>Two</p><p>Three</p>",
            "<p>Before<br>--<br>One<br>Two<br>Three</p>",
            "<div>Before</div>--<br><div>One</div><div>Two</div><div>Three</div>",
        ] {
            let doc = folded(html);
            assert!(!doc.has_folds(), "{html}: {:?}", texts(&doc));
            assert!(doc.document.plain_text().contains("Three"), "{html}");
        }
        // The RFC 3676 delimiter, in a block of its own or before a `<br>`,
        // and written with a no-break space, starts one.
        for html in [
            "<p>Before</p><div>-- </div><p>One</p><p>Two</p><p>Three</p>",
            "<div>Before</div>-- <br><div>One</div><div>Two</div><div>Three</div>",
            "<p>Before<br>--&nbsp;<br>One<br>Two<br>Three</p>",
        ] {
            let doc = folded(html);
            let sections = texts(&doc);
            assert_eq!(
                sections,
                vec![
                    (None, "Before".into()),
                    (Some(Fold::Signature), "One\nTwo\nThree".into())
                ],
                "{html}"
            );
        }
    }

    #[test]
    fn a_senders_own_header_lines_do_not_fold_the_message() {
        // A template with From/Sent/Subject lines but no address.
        let doc = folded(
            "<p>Use this for the ticket:</p><p>From: Support<br>Sent: daily<br>Subject: Your ticket</p><p>Then attach the log.</p>",
        );
        assert!(!doc.has_folds(), "{:?}", texts(&doc));
        // An address, but nothing quoted under it.
        let doc = folded(
            "<p>Notes from the call.</p><p>From: Ada &lt;ada@example.test&gt;<br>Sent: Monday<br>Subject: Plans</p>",
        );
        assert!(!doc.has_folds(), "{:?}", texts(&doc));
        // The same header over the earlier mail folds it.
        let doc = folded(
            "<p>Agreed.</p><p>From: Ada &lt;ada@example.test&gt;<br>Sent: Monday<br>Subject: Plans</p><p>Earlier</p>",
        );
        assert_eq!(texts(&doc)[1].0, Some(Fold::Quote));
    }

    #[test]
    fn outlooks_reply_div_folds_without_an_address() {
        let doc = folded(
            "<div>Agreed.</div><hr><div id=\"divRplyFwdMsg\"><b>From:</b> Ada Lovelace<br><b>Sent:</b> Monday<br><b>Subject:</b> Plans</div><div>Earlier text</div>",
        );
        let sections = texts(&doc);
        assert_eq!(sections[0], (None, "Agreed.".into()));
        assert_eq!(sections[1].0, Some(Fold::Quote));
        assert!(
            sections[1].1.starts_with("From: Ada Lovelace"),
            "{sections:?}"
        );
        assert!(sections[1].1.ends_with("Earlier text"));
        // Over nothing, it is not history.
        let doc = folded(
            "<div>Agreed.</div><div id=\"divRplyFwdMsg\"><b>From:</b> Ada<br><b>Subject:</b> Plans</div>",
        );
        assert!(!doc.has_folds());
        // The older delimiter folds too.
        let doc = folded("<p>Agreed.</p><p>-----Original Message-----<br>From: Ada<br>Earlier</p>");
        assert_eq!(texts(&doc)[1].0, Some(Fold::Quote));
    }

    #[test]
    fn an_outlook_forward_stays_open() {
        let doc = folded(
            "<div>See below.</div><div id=\"divRplyFwdMsg\"><b>From:</b> Ada<br><b>Sent:</b> Monday<br><b>Subject:</b> FW: Plans</div><div>The plan.</div>",
        );
        assert!(!doc.has_folds(), "{:?}", texts(&doc));
        let doc = folded(
            "<p>See below.</p><p>From: Ada &lt;ada@example.test&gt;<br>Sent: Monday<br>Subject: Fwd: Plans</p><p>The plan.</p>",
        );
        assert!(!doc.has_folds(), "{:?}", texts(&doc));
    }

    #[test]
    fn a_forward_under_a_signature_stays_open() {
        let doc = folded(
            "<p>FYI</p><p>-- <br>Jon Ames<br>Fell<br>fell.test</p><div class=\"gmail_quote\"><div class=\"gmail_attr\">---------- Forwarded message ---------<br>From: Ada</div><p>The plan.</p></div>",
        );
        let sections = texts(&doc);
        assert_eq!(sections[0], (None, "FYI".into()));
        assert_eq!(sections[1].0, Some(Fold::Signature));
        assert_eq!(sections[2].0, None, "{sections:?}");
        assert!(sections[2].1.contains("The plan."));
        // The same written as paragraphs, with no quote container.
        let doc = folded(
            "<p>FYI</p><p>-- <br>Jon Ames<br>Fell<br>fell.test</p><p>---------- Forwarded message ---------<br>From: Ada</p><p>The plan.</p>",
        );
        let sections = texts(&doc);
        assert_eq!(sections.len(), 3, "{sections:?}");
        assert_eq!(sections[2].0, None);
        assert!(sections[2].1.ends_with("The plan."));
    }

    #[test]
    fn quoted_from_lines_name_an_address() {
        assert!(is_quoted_from_line("From: Ada <ada@example.test>"));
        assert!(is_quoted_from_line("From: ada@example.test"));
        assert!(is_quoted_from_line("From: Ada [mailto:ada@example.test]"));
        assert!(!is_quoted_from_line("From: the team"));
        assert!(!is_quoted_from_line("From: @ada on chat"));
        assert!(!is_quoted_from_line("Sent: ada@example.test"));
    }

    #[test]
    fn an_apple_mail_quote_takes_its_attribution_line() {
        let doc = folded(
            "<div>Sounds good.</div><div><br><div>On 5 Jan 2026, at 09:00, Ada wrote:</div><blockquote type=\"cite\"><p>Lunch?</p></blockquote></div>",
        );
        let sections = texts(&doc);
        assert_eq!(sections.len(), 2, "{sections:?}");
        assert_eq!(sections[0], (None, "Sounds good.".into()));
        assert_eq!(sections[1].0, Some(Fold::Quote));
        assert!(sections[1].1.starts_with("On 5 Jan 2026"));
        assert!(sections[1].1.ends_with("Lunch?"));
    }

    #[test]
    fn a_message_that_is_all_quote_stays_open() {
        let doc = folded("<blockquote><p>Only the quote.</p></blockquote>");
        assert!(!doc.has_folds());
        assert_eq!(doc.document.plain_text(), "Only the quote.");
    }

    #[test]
    fn a_forward_stays_open() {
        let doc = folded(
            "<div>FYI</div><div class=\"gmail_quote\"><div class=\"gmail_attr\">---------- Forwarded message ---------<br>From: Ada</div><p>The plan.</p></div>",
        );
        assert!(!doc.has_folds());
        assert!(doc.document.plain_text().contains("The plan."));
    }

    #[test]
    fn an_interleaved_reply_folds_only_the_history_after_the_last_answer() {
        let doc = folded(
            "<blockquote><p>First question?</p></blockquote><p>First answer.</p><blockquote><p>Second?</p></blockquote><p>Second answer.</p><p>-- <br>Jon Ames<br>Fell<br>fell.test</p><blockquote><p>Earlier, unanswered.</p></blockquote>",
        );
        let shape: Vec<_> = doc
            .sections
            .iter()
            .map(|section| (section.fold, section.quoted))
            .collect();
        assert_eq!(
            shape,
            vec![
                (None, true),
                (None, false),
                (None, true),
                (None, false),
                (Some(Fold::SignatureAndQuote), false),
            ]
        );
        // The attribution over an answered quote stays with it, open.
        let doc =
            folded("<p>On Monday, Ada wrote:</p><blockquote><p>Lunch?</p></blockquote><p>Yes.</p>");
        let sections = texts(&doc);
        assert_eq!(sections[0].1, "On Monday, Ada wrote:\nLunch?");
        assert!(doc.sections[0].quoted && doc.sections[0].fold.is_none());
        assert_eq!(sections[1], (None, "Yes.".into()));
        // Fine print under the quote is not an answer: the quote folds.
        let doc = folded(
            "<p>Agreed.</p><blockquote><p>Shall we?</p></blockquote><p style=\"font-size:10px\">Confidential.</p>",
        );
        assert_eq!(doc.sections[1].fold, Some(Fold::Quote), "{:?}", texts(&doc));
    }

    #[test]
    fn a_forward_by_its_subject_stays_open() {
        // Outlook: `FW:` on the outer subject, the original's in the header.
        let html = "<div>See below.</div><div>-- <br>Ruth Allard<br>Facilities<br>example.org</div><hr><div id=\"divRplyFwdMsg\"><b>From:</b> Ada &lt;ada@example.test&gt;<br><b>Sent:</b> Monday<br><b>Subject:</b> Plans</div><div>The plan.</div>";
        for subject in [
            "FW: Plans",
            "fwd: Plans",
            "[team] Fwd: Plans",
            "WG: Plans",
            "TR: Plans",
            "RV: Plans",
        ] {
            let doc = fold_history(html_to_document(html), subject);
            let sections = texts(&doc);
            assert_eq!(sections[0], (None, "See below.".into()), "{subject}");
            assert_eq!(sections[1].0, Some(Fold::Signature), "{subject}");
            assert_eq!(sections[2].0, None, "{subject}: {sections:?}");
            assert!(sections[2].1.ends_with("The plan."), "{subject}");
        }
        // The same as a reply folds the earlier mail.
        let doc = fold_history(html_to_document(html), "RE: Plans");
        assert_eq!(
            doc.sections.last().unwrap().fold,
            Some(Fold::SignatureAndQuote)
        );
        // Apple Mail's forward line keeps the quote under it open.
        let doc = folded(
            "<div>FYI</div><div>Begin forwarded message:</div><blockquote type=\"cite\"><div><b>From: </b>Ada</div><p>The plan.</p></blockquote>",
        );
        assert!(!doc.has_folds(), "{:?}", texts(&doc));
    }

    #[test]
    fn sections_cover_every_block_in_order() {
        for fixture in crate::fixtures() {
            let doc = fold_history(
                html_to_document(&fixture.html),
                fixture.headers.get("Subject").unwrap_or_default(),
            );
            let mut next = 0;
            for section in &doc.sections {
                assert_eq!(section.blocks.start, next, "{}", fixture.name);
                assert!(section.blocks.end > section.blocks.start);
                next = section.blocks.end;
            }
            assert_eq!(next, doc.document.blocks.len(), "{}", fixture.name);
        }
    }
}
