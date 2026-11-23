use fell_render::{Block, Inline, ListItem, StyledDocument, html_to_document, plain_to_document};

fn list(block: &Block, expected_order: bool) -> &[ListItem] {
    let Block::List { ordered, items } = block else {
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
