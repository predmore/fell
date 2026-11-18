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
    RenderedBody {
        blocks: split_blocks(&raw),
    }
}

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
        if !joined.trim().is_empty() {
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
        let body = "Thanks.\n\n-- \nCaleb\n\n> older line\n> second";
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
        let signed = render_stored_body(Some("Thanks.\n-- \nCaleb"), "");
        assert!(
            signed
                .blocks
                .iter()
                .any(|block| matches!(block, BodyBlock::Signature(text) if text.contains("Caleb")))
        );
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
