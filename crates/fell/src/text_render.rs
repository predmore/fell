//! Stored-body render seam.
//!
//! The conversation view paints [`RenderedBody`] and does not look at MIME
//! or HTML. [`render_stored_body`] turns the store's plain `body_text` into
//! that model. The HTML milestone replaces this function; the inbox should
//! keep consuming [`RenderedBody`].

/// A message body the conversation view can paint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedBody {
    pub blocks: Vec<BodyBlock>,
}

/// One piece of stored plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyBlock {
    Text(String),
    /// Quoted history. Collapsed until the reader expands it.
    Quote(String),
    /// A signature block. Collapsed until the reader expands it.
    Signature(String),
}

/// Turn stored plain text into blocks. Markup is not interpreted.
pub fn render_stored_body(body_text: Option<&str>, snippet: &str) -> RenderedBody {
    let raw = body_text.unwrap_or(snippet).replace("\r\n", "\n");
    if raw.trim().is_empty() {
        return RenderedBody {
            blocks: vec![BodyBlock::Text(snippet.trim().to_string())],
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
            blocks.push(BodyBlock::Text(joined.trim().to_string()));
        }
    };
    let flush_quote = |quote: &mut Vec<&str>, blocks: &mut Vec<BodyBlock>| {
        if quote.is_empty() {
            return;
        }
        let joined = quote
            .iter()
            .map(|line| line.trim_start_matches('>').trim_start())
            .collect::<Vec<_>>()
            .join("\n");
        quote.clear();
        if !joined.trim().is_empty() {
            blocks.push(BodyBlock::Quote(joined.trim().to_string()));
        }
    };
    let flush_signature = |signature: &mut Vec<&str>, blocks: &mut Vec<BodyBlock>| {
        if signature.is_empty() {
            return;
        }
        let joined = signature.join("\n");
        signature.clear();
        if !joined.trim().is_empty() {
            blocks.push(BodyBlock::Signature(joined.trim().to_string()));
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
        if !in_signature && is_signature_rule(trimmed) {
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
        blocks.push(BodyBlock::Text(raw.trim().to_string()));
    }
    blocks
}

fn is_quote(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('>')
}

fn is_signature_rule(line: &str) -> bool {
    let trimmed = line.trim_end();
    trimmed == "--" || trimmed == "-- "
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
    fn missing_body_uses_the_snippet() {
        let rendered = render_stored_body(None, "just the snippet");
        assert_eq!(
            rendered.blocks,
            vec![BodyBlock::Text("just the snippet".into())]
        );
    }
}
