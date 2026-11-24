use fell_render::{Block, Inline, ListItem, StyledDocument, html_to_document, plain_to_document};

fn list(block: &Block, expected_order: bool) -> &[ListItem] {
    let Block::List { ordered, items, .. } = block else {
        panic!("expected a list, got {block:?}");
    };
    assert_eq!(*ordered, expected_order);
    items
}

fn item_text(item: &ListItem) -> String {
    StyledDocument::blocks_text(&item.blocks)
}

#[test]
fn word_margins_give_typed_bullets_their_depth_and_keep_the_link() {
    let document = html_to_document(include_str!("../fixtures/word-list.html"));
    let [Block::Paragraph { .. }, checklist, Block::Paragraph { .. }] = &document.blocks[..] else {
        panic!("expected prose around the checklist: {document:?}");
    };
    let [first, second] = list(checklist, false) else {
        panic!("expected two top-level bullets");
    };
    let [_, nested] = &first.blocks[..] else {
        panic!("expected a sublist under the first bullet");
    };
    let [verify, second_look] = list(nested, false) else {
        panic!("expected two second-level bullets");
    };
    let [
        Block::Paragraph { inlines },
        Block::Paragraph { inlines: link },
        third_level,
    ] = &verify.blocks[..]
    else {
        panic!("expected the instruction, continuation, and third-level list: {verify:?}");
    };
    assert!(
        inlines
            .iter()
            .any(|inline| matches!(inline, Inline::Strong(_)))
    );
    assert!(
        matches!(&link[..], [Inline::Link { href, .. }] if href.starts_with("https://example.test/"))
    );
    assert_eq!(list(third_level, false).len(), 1);
    assert_eq!(item_text(second_look), "Ask for a second look.");
    assert_eq!(item_text(second), "Publish the guide.");
    assert!(!document.plain_text().contains("\no "));
}

#[test]
fn outlook_levels_create_ordered_lists_and_read_conditional_markers() {
    let document = html_to_document(include_str!("../fixtures/outlook-list.html"));
    let [checklist, Block::Paragraph { .. }] = &document.blocks[..] else {
        panic!("expected a checklist and paragraph: {document:?}");
    };
    let [first, second] = list(checklist, true) else {
        panic!("expected two top-level numbers");
    };
    let [intro, nested] = &first.blocks[..] else {
        panic!("expected a sublist under the first item");
    };
    assert_eq!(
        StyledDocument::blocks_text(std::slice::from_ref(intro)),
        "Open the release checklist."
    );
    let [verify, review] = list(nested, true) else {
        panic!("expected two second-level letters");
    };
    assert_eq!(item_text(verify), "Verify the preview link.");
    assert_eq!(item_text(review), "Review the mobile layout.");
    assert_eq!(item_text(second), "Publish the guide.");
}

#[test]
fn plain_word_padding_and_indentation_create_nested_lists() {
    let document = plain_to_document(include_str!("../fixtures/word-list.txt"));
    let [Block::Paragraph { .. }, checklist, Block::Paragraph { .. }] = &document.blocks[..] else {
        panic!("expected prose around the checklist: {document:?}");
    };
    let [first, second] = list(checklist, false) else {
        panic!("expected two top-level bullets");
    };
    let [_, nested] = &first.blocks[..] else {
        panic!("expected a sublist under the first bullet");
    };
    let [verify, second_look] = list(nested, false) else {
        panic!("expected two second-level bullets");
    };
    assert_eq!(verify.blocks.len(), 2);
    assert!(item_text(verify).starts_with("Verify the preview link.\nhttps://"));
    assert_eq!(item_text(second_look), "Ask for a second look.");
    assert_eq!(item_text(second), "Publish the guide.");
    let indented = plain_to_document("1.\tFirst\r\n  a.  Nested\r\n2.\tLast");
    let [first, _] = list(&indented.blocks[0], true) else {
        panic!("expected two top-level numbers");
    };
    assert_eq!(list(&first.blocks[1], true).len(), 1);
    let same_glyph = plain_to_document("o\tFirst\no\tSecond");
    assert_eq!(list(&same_glyph.blocks[0], false).len(), 2);
}

#[test]
fn ambiguous_markers_remain_prose() {
    let html = "<p>- A prose opening.</p><p style='margin-left:1in'>o is a letter.</p><p>-</p><p>-&nbsp;A standalone unindented line.</p>";
    let document = html_to_document(html);
    assert!(
        document
            .blocks
            .iter()
            .all(|block| matches!(block, Block::Paragraph { .. }))
    );
    assert_eq!(
        document.plain_text(),
        "- A prose opening.\no is a letter.\n-\n-\u{a0}A standalone unindented line."
    );
    let text = "- A prose opening.\n  o is a letter.\n-\n-\u{a0}A standalone unindented line.";
    let document = plain_to_document(text);
    assert!(matches!(&document.blocks[..], [Block::Paragraph { .. }]));
    assert_eq!(document.plain_text(), text);
}

#[test]
fn an_unindented_run_qualifies_and_a_same_level_kind_change_starts_a_new_list() {
    let document = html_to_document(
        "<p>-&nbsp;Check the notes.</p><p>-  Check the editors.</p><p>1.&nbsp;Publish the release.</p>",
    );
    let [bullets, numbers] = &document.blocks[..] else {
        panic!("expected two lists: {document:?}");
    };
    assert_eq!(list(bullets, false).len(), 2);
    assert_eq!(list(numbers, true).len(), 1);
}

#[test]
fn year_headings_and_long_numbers_keep_the_original_text() {
    for padding in ["&nbsp;", "  "] {
        let html = format!(
            "<p>2024.{padding}We shipped the reader.</p><p>2025.{padding}We shipped search.</p>\
             <p style='margin-left:0.5in'>2026.{padding}We shipped drafts.</p>"
        );
        let doc = html_to_document(&html);
        assert!(
            doc.blocks
                .iter()
                .all(|block| matches!(block, Block::Paragraph { .. }))
        );
        assert!(doc.plain_text().contains("2024."));
        assert!(doc.plain_text().contains("2025."));
        assert!(doc.plain_text().contains("2026."));
    }

    for padding in ["\u{a0}", "  ", "\t"] {
        let text = format!(
            "2024.{padding}We shipped the reader.\n2025.{padding}We shipped search.\n  91234.{padding}Work remains."
        );
        let doc = plain_to_document(&text);
        assert!(matches!(&doc.blocks[..], [Block::Paragraph { .. }]));
        assert_eq!(doc.plain_text(), text);
    }

    let doc =
        plain_to_document("  999.\tLast supported ordinal.\n  1000.\tKeep this line as text.");
    assert_eq!(list(&doc.blocks[0], true).len(), 1);
    assert_eq!(
        doc.plain_text(),
        "999. Last supported ordinal.\n1000.\tKeep this line as text."
    );
}

#[test]
fn ordered_labels_keep_their_start_and_style_in_both_formats() {
    use fell_render::ListLabelStyle::*;

    let html = "<p>3.&nbsp;Review the guide.</p><p style='margin-left:1in'>a.&nbsp;Check the layout.</p>\
                <p style='margin-left:1in'>b.&nbsp;Proofread the text.</p><p>4.&nbsp;Share it.</p>\
                <p>Later</p><p>10.&nbsp;Publish it.</p><p>11.&nbsp;Announce it.</p>";
    let plain = "3.\tReview the guide.\n    a.\tCheck the layout.\n    b.\tProofread the text.\n4.\tShare it.\n\nLater\n\n10.\tPublish it.\n11.\tAnnounce it.";
    for doc in [html_to_document(html), plain_to_document(plain)] {
        let [first, Block::Paragraph { .. }, second] = &doc.blocks[..] else {
            panic!("expected two lists around prose: {doc:?}");
        };
        assert!(matches!(
            first,
            Block::List {
                start: 3,
                style: Decimal,
                ..
            }
        ));
        let sublist = &list(first, true)[0].blocks[1];
        assert!(matches!(
            sublist,
            Block::List {
                start: 1,
                style: LowerAlpha,
                ..
            }
        ));
        assert!(matches!(
            second,
            Block::List {
                start: 10,
                style: Decimal,
                ..
            }
        ));
        assert_eq!(
            doc.plain_text(),
            "3. Review the guide.\na. Check the layout.\nb. Proofread the text.\n4. Share it.\nLater\n10. Publish it.\n11. Announce it."
        );
    }

    let doc = plain_to_document(
        "C.\tCapital C\nD.\tCapital D\ni.\tOne\nii.\tTwo\niii.\tThree\niv.\tFour\nv.\tFive\n8.\tEight\n11.\tEleven",
    );
    assert!(
        matches!(
            doc.blocks.as_slice(),
            [
                Block::List {
                    start: 3,
                    style: UpperAlpha,
                    ..
                },
                Block::List {
                    start: 1,
                    style: LowerRoman,
                    ..
                },
                Block::List {
                    start: 8,
                    style: Decimal,
                    ..
                },
                Block::List {
                    start: 11,
                    style: Decimal,
                    ..
                }
            ]
        ),
        "{doc:?}"
    );
    assert!(
        doc.plain_text()
            .contains("iv. Four\nv. Five\n8. Eight\n11. Eleven")
    );
}

#[test]
fn plain_continuations_follow_the_text_column() {
    let body = "-\tFirst\n-\tSecond\n A separate note";
    let doc = plain_to_document(body);
    let [checklist, Block::Paragraph { .. }] = &doc.blocks[..] else {
        panic!("expected the note to stay outside the list: {doc:?}");
    };
    assert_eq!(item_text(&list(checklist, false)[1]), "Second");

    let body = "-\u{a0}Parent\no\tChild\n  A note under the parent\n-\tNext";
    let doc = plain_to_document(body);
    let [parent, next] = list(&doc.blocks[0], false) else {
        panic!("expected two top-level bullets: {doc:?}");
    };
    assert_eq!(item_text(&list(&parent.blocks[1], false)[0]), "Child");
    assert_eq!(
        StyledDocument::blocks_text(&parent.blocks[2..]),
        "A note under the parent"
    );
    assert_eq!(item_text(next), "Next");

    let body = "-\u{a0}Parent\no\tChild\n A separate note";
    let doc = plain_to_document(body);
    assert!(matches!(
        doc.blocks.as_slice(),
        [Block::List { .. }, Block::Paragraph { .. }]
    ));
}

#[test]
fn html_blank_paragraphs_and_parent_continuations_keep_the_list_open() {
    let doc = html_to_document(
        "<p style='margin-left:.25in;mso-list:l0 level1 lfo1'>1.&nbsp;Parent</p>\
         <p style='margin-left:.5in;mso-list:l0 level2 lfo1'>a.&nbsp;Child</p>\
         <p style='margin-left:.25in'><a href='https://example.test/'>Parent link</a></p>\
         <p>&nbsp;</p><p><br></p>\
         <p style='margin-left:.25in;mso-list:l0 level1 lfo1'>2.&nbsp;Next</p>",
    );
    let [checklist] = &doc.blocks[..] else {
        panic!("expected one list: {doc:?}");
    };
    let [parent, next] = list(checklist, true) else {
        panic!("expected two top-level items: {doc:?}");
    };
    assert_eq!(item_text(&list(&parent.blocks[1], true)[0]), "Child");
    assert!(matches!(
        &parent.blocks[2],
        Block::Paragraph { inlines } if matches!(&inlines[..], [Inline::Link { .. }])
    ));
    assert_eq!(item_text(next), "Next");
    assert_eq!(
        doc.plain_text(),
        "1. Parent\na. Child\nParent link\n2. Next"
    );
}
