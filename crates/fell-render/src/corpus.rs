//! Invented mail in the shapes real clients and senders write.
//!
//! Each mail mirrors one family's markup: Gmail's per-line `<div>`s and
//! nested `gmail_quote`, Apple Mail's `blockquote type="cite"`, Outlook's
//! `MsoNormal` paragraphs and `#divRplyFwdMsg`, Proton, Thunderbird, Yahoo,
//! an ESP sale and receipt, a wide data table, `<pre>` code, a
//! right-to-left reply, interleaved replies, forwards, a pasted photo, a
//! signature logo, a CJK note, a calendar invite, `>`-quoted lines and a
//! sign-in code. The content is invented and every address is on an
//! `example` domain. Each has a checked-in outline of what the conversation
//! paints (`fixtures/corpus/<name>.outline`); `FELL_UPDATE_SNAPSHOTS=1`
//! rewrites them.

use crate::choose::{MailHeaders, choose_view};
use crate::fixtures::Fixture;

struct Embedded {
    name: &'static str,
    html: &'static str,
    headers: &'static str,
}

macro_rules! corpus {
    ($($name:literal),* $(,)?) => {
        &[$(Embedded {
            name: $name,
            html: include_str!(concat!("../fixtures/corpus/", $name, ".html")),
            headers: include_str!(concat!("../fixtures/corpus/", $name, ".headers")),
        }),*]
    };
}

const CORPUS: &[Embedded] = corpus!(
    "gmail-reply-nested",
    "apple-mac-reply",
    "apple-iphone-reply",
    "outlook-reply",
    "outlook-new",
    "proton-reply",
    "thunderbird-reply",
    "yahoo-reply",
    "esp-marketing",
    "esp-receipt",
    "esp-digest",
    "data-table",
    "pre-code",
    "rtl-reply",
    "thunderbird-inline-reply",
    "gmail-inline-reply",
    "outlook-forward",
    "apple-forward",
    "personal-photo",
    "outlook-signature-logo",
    "cjk-note",
    "calendar-invite",
    "webmail-gt-reply",
    "otp-code",
);

/// The corpus, in a stable order.
pub fn corpus() -> Vec<Fixture> {
    CORPUS
        .iter()
        .map(|embedded| {
            let headers = MailHeaders::from_lines(embedded.headers);
            let expected = choose_view(&headers, embedded.html);
            Fixture {
                name: embedded.name.to_string(),
                headers,
                html: embedded.html.to_string(),
                expected,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod outline {
    //! A plain-text picture of a folded document, for snapshots.

    use crate::document::{Align, Block, ImageSource, Inline};
    use crate::history::{Fold, FoldedDocument};

    pub fn outline(doc: &FoldedDocument) -> String {
        let mut out = String::new();
        for section in &doc.sections {
            let name = match section.fold {
                None if section.quiet => "quiet",
                None if section.quoted => "open quoted",
                None => "open",
                Some(Fold::Quote) => "fold quote",
                Some(Fold::Signature) => "fold signature",
                Some(Fold::SignatureAndQuote) => "fold signature and quote",
                Some(Fold::QuoteAndSignature) => "fold quote and signature",
            };
            out.push_str(&format!("## {name}\n"));
            for block in doc.blocks(section) {
                push_block(&mut out, block, 0);
            }
        }
        out
    }

    fn line(out: &mut String, depth: usize, text: &str) {
        out.push_str(&"  ".repeat(depth));
        out.push_str(text);
        out.push('\n');
    }

    /// `tag` and the inlines; each line break continues under ` | `.
    fn text_block(out: &mut String, depth: usize, tag: &str, inlines: &[Inline]) {
        let text = inline_text(inlines);
        let mut lines = text.split('\n');
        line(
            out,
            depth,
            &format!("{tag} {}", lines.next().unwrap_or_default()),
        );
        for rest in lines {
            line(out, depth, &format!("{} | {rest}", " ".repeat(tag.len())));
        }
    }

    fn push_block(out: &mut String, block: &Block, depth: usize) {
        match block {
            Block::Heading { level, inlines } => {
                text_block(out, depth, &format!("h{level}"), inlines)
            }
            Block::Paragraph { inlines } => text_block(out, depth, "p", inlines),
            Block::List { ordered, items } => {
                line(out, depth, if *ordered { "ol" } else { "ul" });
                for item in items {
                    line(out, depth + 1, "-");
                    for child in &item.blocks {
                        push_block(out, child, depth + 2);
                    }
                }
            }
            Block::Quote { blocks } => {
                line(out, depth, "quote");
                for child in blocks {
                    push_block(out, child, depth + 1);
                }
            }
            Block::Code { text, large } => {
                line(out, depth, if *large { "code large" } else { "code" });
                for code in text.lines() {
                    line(out, depth, &format!("   | {code}"));
                }
            }
            Block::Table {
                header,
                rows,
                align,
            } => {
                let align: Vec<&str> = align
                    .iter()
                    .map(|align| match align {
                        Align::Start => "start",
                        Align::Center => "center",
                        Align::End => "end",
                    })
                    .collect();
                line(out, depth, &format!("table [{}]", align.join(", ")));
                if !header.is_empty() {
                    line(out, depth + 1, &format!("th {}", cells(header)));
                }
                for row in rows {
                    line(out, depth + 1, &format!("td {}", cells(row)));
                }
            }
            Block::Image { source, alt } => line(out, depth, &image(source, alt)),
            Block::ThematicBreak => line(out, depth, "hr"),
            Block::Small { blocks } => {
                line(out, depth, "small");
                for child in blocks {
                    push_block(out, child, depth + 1);
                }
            }
            Block::ReplyHeader { blocks } => {
                line(out, depth, "reply-header");
                for child in blocks {
                    push_block(out, child, depth + 1);
                }
            }
        }
    }

    fn cells(row: &[Vec<Block>]) -> String {
        row.iter()
            .map(|cell| {
                let mut text = String::new();
                for (at, block) in cell.iter().enumerate() {
                    if at > 0 {
                        text.push_str(" / ");
                    }
                    let mut inner = String::new();
                    push_block(&mut inner, block, 0);
                    text.push_str(inner.trim_end().replace('\n', " ").as_str());
                }
                text
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn image(source: &ImageSource, alt: &str) -> String {
        let kind = match source {
            ImageSource::Remote(_) => "remote",
            ImageSource::Cid(_) => "cid",
            ImageSource::Data { .. } => "data",
        };
        format!("![{alt}]({kind})")
    }

    fn inline_text(inlines: &[Inline]) -> String {
        let mut out = String::new();
        for inline in inlines {
            match inline {
                Inline::Text(text) => out.push_str(&text.replace('\u{a0}', "␣")),
                Inline::Strong(children) => out.push_str(&format!("**{}**", inline_text(children))),
                Inline::Emphasis(children) => out.push_str(&format!("*{}*", inline_text(children))),
                Inline::Strike(children) => out.push_str(&format!("~~{}~~", inline_text(children))),
                Inline::Code(text) => out.push_str(&format!("`{text}`")),
                Inline::Link { href, children } => {
                    out.push_str(&format!("[{}]({href})", inline_text(children)))
                }
                Inline::Image { source, alt } => out.push_str(&image(source, alt)),
                Inline::LineBreak => out.push('\n'),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{FoldedDocument, fold_history};

    /// What the conversation paints: bulk mail is not folded.
    pub(crate) fn folded(fixture: &Fixture) -> FoldedDocument {
        let prepared = crate::prepare(&fixture.html, &fixture.headers);
        if fixture.headers.is_bulk() {
            FoldedDocument::unfolded(prepared.document)
        } else {
            fold_history(
                prepared.document,
                fixture.headers.get("Subject").unwrap_or_default(),
            )
        }
    }

    #[test]
    fn every_corpus_mail_matches_its_outline() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/corpus");
        let update = std::env::var_os("FELL_UPDATE_SNAPSHOTS").is_some();
        let mut stale = Vec::new();
        for fixture in corpus() {
            let got = outline::outline(&folded(&fixture));
            let path = dir.join(format!("{}.outline", fixture.name));
            if update {
                std::fs::write(&path, &got).unwrap();
                continue;
            }
            let want = std::fs::read_to_string(&path).unwrap_or_default();
            if want != got {
                stale.push(format!("--- {}\n{got}", fixture.name));
            }
        }
        assert!(
            stale.is_empty(),
            "outlines changed (FELL_UPDATE_SNAPSHOTS=1 to accept):\n{}",
            stale.join("\n")
        );
    }

    #[test]
    fn the_corpus_is_invented_mail_on_example_domains() {
        let corpus = corpus();
        assert!(corpus.len() >= 12);
        for fixture in &corpus {
            for text in [&fixture.html, &format!("{:?}", fixture.headers)] {
                for word in text.split(|ch: char| ch.is_whitespace() || "<>\"'();,&".contains(ch)) {
                    // A `cid:` names an attachment, not an address.
                    if word.starts_with("cid:") {
                        continue;
                    }
                    let Some((_, domain)) = word.split_once('@') else {
                        continue;
                    };
                    let domain = domain.trim_end_matches(['.', ':']);
                    if domain.is_empty() || !domain.contains('.') {
                        continue;
                    }
                    assert!(
                        domain.ends_with(".example")
                            || domain.ends_with(".test")
                            || domain == "example.com"
                            || domain == "example.net"
                            || domain == "example.org"
                            || domain.ends_with(".example.com")
                            || domain.ends_with(".example.net")
                            || domain.ends_with(".example.org"),
                        "{}: {domain}",
                        fixture.name
                    );
                }
            }
        }
    }

    fn mail(name: &str) -> FoldedDocument {
        let fixture = corpus()
            .into_iter()
            .find(|fixture| fixture.name == name)
            .expect(name);
        folded(&fixture)
    }

    fn paragraphs(blocks: &[Block]) -> Vec<&Vec<Inline>> {
        let mut out = Vec::new();
        for block in blocks {
            match block {
                Block::Paragraph { inlines } => out.push(inlines),
                Block::Quote { blocks } | Block::Small { blocks } => out.extend(paragraphs(blocks)),
                _ => {}
            }
        }
        out
    }

    use crate::document::{Align, Block, Inline};
    use crate::history::Fold;

    #[test]
    fn a_space_between_inline_elements_is_one_space() {
        let doc = mail("esp-marketing");
        let prices: Vec<_> = paragraphs(&doc.document.blocks)
            .into_iter()
            .filter(|inlines| matches!(inlines.first(), Some(Inline::Strike(_))))
            .collect();
        assert_eq!(prices.len(), 2, "{:?}", doc.document.blocks);
        assert_eq!(
            prices[0],
            &vec![
                Inline::Strike(vec![Inline::Text("$180".into())]),
                Inline::Text(" ".into()),
                Inline::Strong(vec![Inline::Text("$126".into())]),
            ]
        );
        let text = doc.document.plain_text();
        assert!(
            text.contains("$180 $126") && text.contains("$90 $63"),
            "{text}"
        );
        // No line starts with the space after a `<br>`.
        assert!(text.contains("\nNorthpine Outfitters, 100"), "{text}");
        assert!(!text.lines().any(|line| line.starts_with(' ')), "{text:?}");
        // A space inside a link or bold run is kept.
        let doc = crate::html_to_document(
            "<p>Read<a href=\"https://a.test\"> the story</a> <b>now </b>please.</p>",
        );
        assert_eq!(doc.plain_text(), "Read the story now please.");
    }

    #[test]
    fn a_layout_table_keeps_its_cells_apart() {
        let doc = mail("esp-marketing");
        let text = doc.document.plain_text();
        assert!(
            text.contains("Free shipping over $75 · Free returns for members · Repairs for life"),
            "{text}"
        );
        // Two rows of short cells in a presentation table are data.
        let receipt = mail("esp-receipt");
        let table = receipt
            .document
            .blocks
            .iter()
            .find_map(|block| match block {
                Block::Table { rows, align, .. } => Some((rows, align)),
                _ => None,
            })
            .expect("the item table");
        assert_eq!(table.1, &vec![Align::Start, Align::End]);
        assert_eq!(table.0.len(), 5);
        assert!(!receipt.document.plain_text().contains("Island$24"));
    }

    #[test]
    fn gmail_and_outlook_lines_are_one_paragraph() {
        let doc = mail("gmail-reply-nested");
        let open = doc.blocks(&doc.sections[0]);
        let Block::Paragraph { inlines } = &open[1] else {
            panic!("{open:?}");
        };
        let lines = inlines
            .iter()
            .filter(|inline| matches!(inline, Inline::LineBreak))
            .count();
        assert_eq!(lines, 2, "three lines, one paragraph: {inlines:?}");
        assert_eq!(open.len(), 4, "{open:?}");
        let outlook = mail("outlook-reply");
        let open = outlook.blocks(&outlook.sections[0]);
        assert_eq!(open.len(), 3, "{open:?}");
        let word = mail("outlook-new");
        assert!(
            word.document
                .plain_text()
                .starts_with("Hello,\nThe heating")
        );
        assert_eq!(
            paragraphs(&word.document.blocks)[4],
            &vec![
                Inline::Text("Thanks,".into()),
                Inline::LineBreak,
                Inline::Text("Pia".into())
            ]
        );
    }

    #[test]
    fn decorative_images_go_and_blocked_ones_share_a_line() {
        let doc = mail("esp-marketing");
        let debug = format!("{:?}", doc.document);
        assert!(!debug.contains("hero-winter"), "alt=\"\" hero");
        assert!(!debug.contains("icon-truck"), "alt=\"\" icon");
        let social = paragraphs(&doc.document.blocks)
            .into_iter()
            .find(|inlines| {
                crate::html::is_image_row(inlines) && format!("{inlines:?}").contains("Social feed")
            })
            .expect("one row of social icons");
        assert!(format!("{social:?}").contains("Videos"));
        // Stacked blocked images join one line too.
        let doc = crate::html_to_document(
            "<p>Hi</p><img src=\"https://a.test/1.png\" alt=\"One\"><br><a href=\"https://a.test\"><img src=\"https://a.test/2.png\" alt=\"Two\"></a>&nbsp;<img src=\"https://a.test/3.png\" alt=\"Three\"><div><img src=\"https://a.test/4.png\" alt=\"Four\"></div>",
        );
        assert_eq!(doc.blocks.len(), 2, "{:?}", doc.blocks);
    }

    #[test]
    fn font_size_and_face_say_heading_and_code() {
        let doc = mail("esp-marketing");
        let headings: Vec<(u8, String)> = doc
            .document
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Heading { level, inlines } => {
                    Some((*level, StyledDoc(inlines.clone()).text()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(headings[0].0, 2);
        assert!(headings[0].1.starts_with("The cold is coming"));
        assert_eq!(headings[1], (3, "Ridge Parka".into()));
        let receipt = mail("esp-receipt");
        assert!(
            receipt
                .document
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Code { text, .. } if text == "BX7-42QK"))
        );
    }

    struct StyledDoc(Vec<Inline>);

    impl StyledDoc {
        fn text(self) -> String {
            crate::StyledDocument {
                blocks: vec![Block::Paragraph { inlines: self.0 }],
            }
            .plain_text()
        }
    }

    #[test]
    fn adjacent_folds_are_one_and_phone_signatures_show_quietly() {
        let gmail = mail("gmail-reply-nested");
        let folds: Vec<_> = gmail.sections.iter().map(|section| section.fold).collect();
        assert_eq!(folds, [None, Some(Fold::SignatureAndQuote)]);
        let thunderbird = mail("thunderbird-reply");
        assert_eq!(thunderbird.sections[1].fold, Some(Fold::QuoteAndSignature));
        let iphone = mail("apple-iphone-reply");
        let quiet = &iphone.sections[1];
        assert!(quiet.quiet && quiet.fold.is_none());
        assert_eq!(
            crate::StyledDocument {
                blocks: iphone.blocks(quiet).to_vec()
            }
            .plain_text(),
            "Sent from my iPhone"
        );
        assert_eq!(iphone.sections[2].fold, Some(Fold::Quote));
        let yahoo = mail("yahoo-reply");
        assert_eq!(yahoo.sections[1].fold, Some(Fold::Quote));
    }
}
