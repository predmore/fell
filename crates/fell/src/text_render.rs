//! Stored-body render seam.
//!
//! The conversation view paints [`RenderedBody`] and does not look at MIME
//! or HTML. [`render_stored_body`] turns the store's plain `body_text` into
//! that model. The HTML milestone replaces this function; the inbox should
//! keep consuming [`RenderedBody`].

use gpui::SharedString;

/// A message body the conversation view can paint.
///
/// Block text is shared. Redraws clone the handle, not the body bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedBody {
    pub blocks: Vec<BodyBlock>,
}

/// One piece of stored plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyBlock {
    Text(SharedString),
    /// Quoted history. Collapsed until the reader expands it.
    Quote(SharedString),
    /// A signature block. Collapsed until the reader expands it.
    Signature(SharedString),
    /// A compose draft's markdown, painted with the compose window's
    /// renderer, so the draft reads as the recipient will see it. `agent`:
    /// an agent wrote it, so each link shows where it goes.
    Markdown {
        doc: std::sync::Arc<fell_core::compose::StyledDoc>,
        agent: bool,
    },
}

fn shared(text: &str) -> SharedString {
    SharedString::from(text.to_string())
}

/// Turn stored plain text into blocks. Markup is not interpreted.
pub fn render_stored_body(body_text: Option<&str>, snippet: &str) -> RenderedBody {
    let raw = body_text.unwrap_or(snippet).replace("\r\n", "\n");
    if raw.trim().is_empty() {
        return RenderedBody {
            blocks: vec![BodyBlock::Text(shared(snippet.trim()))],
        };
    }
    let (head, history) = split_outlook_history(&raw);
    let mut blocks = split_blocks(head);
    if let Some(history) = history {
        blocks.push(BodyBlock::Quote(shared(history.trim())));
    }
    RenderedBody { blocks }
}

/// Outlook quotes no `>`: under the reply it writes a `From:` / `Sent:` /
/// `Subject:` header (or `-----Original Message-----`) and then the earlier
/// mail. Everything from that header on is history. The `From:` line must
/// name an address, as a reply header's does, so a sender's own template
/// lines stay; a forward (`Subject: FW:`) stays open. Nothing is split off
/// when no reply text would be left above it, or no quoted text under it.
fn split_outlook_history(raw: &str) -> (&str, Option<&str>) {
    let lines: Vec<(usize, &str)> = raw
        .split_inclusive('\n')
        .scan(0, |at, line| {
            let start = *at;
            *at += line.len();
            Some((start, line))
        })
        .collect();
    for (index, (start, line)) in lines.iter().enumerate() {
        let line = line.trim();
        let header_end = if line.starts_with("-----Original Message-----") {
            Some(index + 1)
        } else if fell_render::is_quoted_from_line(line) {
            let next = &lines[index..(index + 6).min(lines.len())];
            let has = |prefix: &str| next.iter().any(|(_, l)| l.trim().starts_with(prefix));
            let subject = next
                .iter()
                .position(|(_, l)| l.trim().starts_with("Subject:"));
            match subject {
                Some(at) if has("Sent:") || has("Date:") => {
                    let value = next[at].1.trim()["Subject:".len()..]
                        .trim()
                        .to_ascii_lowercase();
                    if value.starts_with("fw:") || value.starts_with("fwd:") {
                        return (raw, None);
                    }
                    Some(index + at + 1)
                }
                _ => None,
            }
        } else {
            None
        };
        let Some(header_end) = header_end else {
            continue;
        };
        let head = &raw[..*start];
        let quoted = lines[header_end..]
            .iter()
            .any(|(_, l)| !l.trim().is_empty());
        if head.trim().is_empty() || !quoted {
            return (raw, None);
        }
        return (head, Some(&raw[*start..]));
    }
    (raw, None)
}

/// Longest signature shown in place, as the HTML path does.
const INLINE_SIGNATURE_LINES: usize = 2;

fn split_blocks(raw: &str) -> Vec<BodyBlock> {
    let mut blocks = Vec::new();
    let mut text: Vec<&str> = Vec::new();
    let mut quote: Vec<&str> = Vec::new();
    let mut signature: Vec<&str> = Vec::new();
    let mut in_signature = false;

    let flush_text = |text: &mut Vec<&str>, blocks: &mut Vec<BodyBlock>| {
        let joined = text.join("\n");
        text.clear();
        if !joined.trim().is_empty() {
            blocks.push(BodyBlock::Text(shared(joined.trim())));
        }
    };
    let flush_quote = |quote: &mut Vec<&str>, blocks: &mut Vec<BodyBlock>| {
        if quote.is_empty() {
            return;
        }
        let joined = quote
            .iter()
            .map(|line| strip_one_quote_marker(line))
            .collect::<Vec<_>>()
            .join("\n");
        quote.clear();
        if !joined.trim().is_empty() {
            blocks.push(BodyBlock::Quote(shared(joined.trim())));
        }
    };
    let flush_signature = |signature: &mut Vec<&str>, blocks: &mut Vec<BodyBlock>| {
        if signature.is_empty() {
            return;
        }
        let joined = signature.join("\n");
        signature.clear();
        let lines = joined
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count();
        if lines == 0 {
            return;
        }
        // A name and a line under it read faster than the fold hiding them.
        if lines <= INLINE_SIGNATURE_LINES {
            blocks.push(BodyBlock::Text(shared(joined.trim())));
        } else {
            blocks.push(BodyBlock::Signature(shared(joined.trim())));
        }
    };

    for line in raw.lines() {
        let trimmed = line.trim_end();
        if is_quote(trimmed) {
            flush_text(&mut text, &mut blocks);
            flush_signature(&mut signature, &mut blocks);
            in_signature = false;
            quote.push(trimmed);
            continue;
        }
        if !in_signature && is_signature_rule(line) {
            flush_text(&mut text, &mut blocks);
            flush_quote(&mut quote, &mut blocks);
            in_signature = true;
            continue;
        }
        if in_signature {
            signature.push(trimmed);
        } else {
            flush_quote(&mut quote, &mut blocks);
            text.push(trimmed);
        }
    }
    flush_text(&mut text, &mut blocks);
    flush_quote(&mut quote, &mut blocks);
    flush_signature(&mut signature, &mut blocks);
    if blocks.is_empty() {
        blocks.push(BodyBlock::Text(shared(raw.trim())));
    }
    blocks
}

fn is_quote(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

/// Drop one quote marker and at most one space after it, so `>>` stays nested.
fn strip_one_quote_marker(line: &str) -> String {
    let rest = line.trim_start();
    let indent_len = line.len() - rest.len();
    let Some(rest) = rest.strip_prefix('>') else {
        return line.to_string();
    };
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    format!("{}{rest}", &line[..indent_len])
}

/// RFC 3676 signature delimiter: two dashes, one space, then only whitespace.
fn is_signature_rule(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("-- ") else {
        return false;
    };
    rest.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_parsed_as_html() {
        let rendered = render_stored_body(Some("Hello <b>there</b>"), "snippet");
        match &rendered.blocks[..] {
            [BodyBlock::Text(text)] => {
                assert!(text.contains("<b>there</b>"));
                assert!(!text.contains("**"));
            }
            other => panic!("expected one text block, got {other:?}"),
        }
    }

    #[test]
    fn quotes_and_signature_are_separate_blocks() {
        let body = "Thanks.\n\n-- \nCaleb\nFell\nfell.test\n\n> older line\n> second";
        let rendered = render_stored_body(Some(body), "");
        assert!(matches!(rendered.blocks[0], BodyBlock::Text(_)));
        assert!(
            rendered
                .blocks
                .iter()
                .any(|block| matches!(block, BodyBlock::Signature(text) if text.contains("Caleb")))
        );
        assert!(
            rendered.blocks.iter().any(
                |block| matches!(block, BodyBlock::Quote(text) if text.contains("older line"))
            )
        );
    }

    #[test]
    fn a_short_signature_shows_in_place() {
        let rendered = render_stored_body(Some("Thanks.\n\n-- \nCaleb\nFell"), "");
        assert!(
            rendered
                .blocks
                .iter()
                .all(|block| matches!(block, BodyBlock::Text(_))),
            "{:?}",
            rendered.blocks
        );
        assert!(
            matches!(&rendered.blocks[..], [_, BodyBlock::Text(text)] if text == "Caleb\nFell")
        );
    }

    #[test]
    fn nested_quotes_keep_one_level() {
        let body = "Thanks.\n\n>> earlier reply\n> > still nested\n> top level\n>  two spaces";
        let rendered = render_stored_body(Some(body), "");
        let quote = rendered
            .blocks
            .iter()
            .find_map(|block| match block {
                BodyBlock::Quote(text) => Some(text.as_str()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            quote,
            "> earlier reply\n> still nested\ntop level\n two spaces"
        );
    }

    #[test]
    fn plain_dashes_do_not_hide_the_rest() {
        let rendered = render_stored_body(Some("diff\n--\nstill visible"), "");
        assert!(
            rendered
                .blocks
                .iter()
                .all(|block| !matches!(block, BodyBlock::Signature(_)))
        );
        let text = rendered
            .blocks
            .iter()
            .find_map(|block| match block {
                BodyBlock::Text(text) => Some(text.as_ref()),
                _ => None,
            })
            .unwrap();
        assert!(text.contains("still visible"));
        let signed = render_stored_body(Some("Thanks.\n-- \nCaleb\nFell\nfell.test"), "");
        assert!(
            signed
                .blocks
                .iter()
                .any(|block| matches!(block, BodyBlock::Signature(text) if text.contains("Caleb")))
        );
    }

    #[test]
    fn an_outlook_header_folds_the_rest() {
        let body = "Amen\n\nGet Outlook for Android\n\nFrom: Ada <ada@example.test>\nSent: Monday, January 5, 2026 9:00 AM\nTo: Jon\nSubject: Re: Plans\n\nEarlier text\n\nFrom: Jon\nSent: Sunday\nSubject: Plans\n\nEven earlier";
        let rendered = render_stored_body(Some(body), "");
        assert_eq!(rendered.blocks.len(), 2, "{:?}", rendered.blocks);
        assert!(matches!(&rendered.blocks[0], BodyBlock::Text(text) if text.starts_with("Amen")));
        assert!(matches!(&rendered.blocks[1], BodyBlock::Quote(text)
            if text.starts_with("From: Ada") && text.ends_with("Even earlier")));
        // A From: line that is not a header stays.
        let note = render_stored_body(Some("From: the team, thanks.\nSee you."), "");
        assert!(matches!(&note.blocks[..], [BodyBlock::Text(_)]));
        // Header lines a sender typed, with no address, stay.
        let template = render_stored_body(
            Some(
                "Use this:\n\nFrom: Support\nSent: daily\nSubject: Your ticket\n\nThen attach the log.",
            ),
            "",
        );
        assert!(
            matches!(&template.blocks[..], [BodyBlock::Text(_)]),
            "{:?}",
            template.blocks
        );
        // A header over nothing quotes nothing.
        let empty = render_stored_body(
            Some("Notes.\n\nFrom: Ada <ada@example.test>\nSent: Mon\nSubject: x\n"),
            "",
        );
        assert!(
            matches!(&empty.blocks[..], [BodyBlock::Text(_)]),
            "{:?}",
            empty.blocks
        );
        // A forward stays open.
        let forward = render_stored_body(
            Some(
                "See below.\n\nFrom: Ada <ada@example.test>\nSent: Mon\nSubject: FW: Plans\n\nThe plan.",
            ),
            "",
        );
        assert!(
            matches!(&forward.blocks[..], [BodyBlock::Text(_)]),
            "{:?}",
            forward.blocks
        );
        // Nothing above the header: nothing is folded.
        let bare = render_stored_body(Some("From: Ada\nSent: Mon\nSubject: x\n\nHi"), "");
        assert!(matches!(&bare.blocks[..], [BodyBlock::Text(_)]));
    }

    #[test]
    fn missing_body_uses_the_snippet() {
        let rendered = render_stored_body(None, "just the snippet");
        assert_eq!(
            rendered.blocks,
            vec![BodyBlock::Text("just the snippet".into())]
        );
    }
}
