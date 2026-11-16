use std::ops::Range;
use std::sync::Arc;

use fell_render::{Block, ImageSource, Inline, ListItem, StyledDocument};
use gpui::{
    AnyElement, FontStyle, FontWeight, HighlightStyle, IntoElement, RenderImage, SharedString,
    StyledText, UnderlineStyle, div, img, prelude::*, px,
};
use image::Frame;
use smallvec::SmallVec;

use super::theme::{Palette, ink, paint};

/// Native styled text. Compose paints markdown through this same function.
pub fn document_column(
    document: &StyledDocument,
    palette: &Palette,
    allow_remote: bool,
) -> gpui::Div {
    let mut column = div().flex().flex_col().gap_4();
    if document.is_empty() {
        return column.child(
            div()
                .text_color(paint(palette.dimmed))
                .child("This message has no body."),
        );
    }
    for block in &document.blocks {
        column = column.child(render_block(block, palette, allow_remote));
    }
    column
}

fn render_block(block: &Block, palette: &Palette, allow_remote: bool) -> AnyElement {
    match block {
        Block::Heading { level, inlines } => {
            let mut heading = div().font_weight(FontWeight::BOLD);
            heading = match level {
                1 => heading.text_xl(),
                2 => heading.text_lg(),
                _ => heading.text_base(),
            };
            heading
                .flex()
                .flex_col()
                .gap_1()
                .children(render_inlines(inlines, palette, allow_remote))
                .into_any_element()
        }
        Block::Paragraph { inlines } => div()
            .flex()
            .flex_col()
            .gap_2()
            .children(render_inlines(inlines, palette, allow_remote))
            .into_any_element(),
        Block::Quote { blocks } => div()
            .border_l(px(2.))
            .border_color(paint(palette.quote))
            .pl_3()
            .text_color(paint(palette.dimmed))
            .flex()
            .flex_col()
            .gap_2()
            .children(
                blocks
                    .iter()
                    .map(|child| render_block(child, palette, allow_remote)),
            )
            .into_any_element(),
        Block::List { ordered, items } => div()
            .flex()
            .flex_col()
            .gap_2()
            .children(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| list_item(index, *ordered, item, palette, allow_remote)),
            )
            .into_any_element(),
        Block::Code { text } => div()
            .font_family("Liberation Mono")
            .text_sm()
            .bg(paint(palette.code_bg))
            .p_3()
            .rounded(px(4.))
            .child(text.clone())
            .into_any_element(),
        Block::Table { header, rows } => render_table(header, rows, palette, allow_remote),
        Block::Image { source, alt } => image_el(source, alt, palette, allow_remote),
        Block::ThematicBreak => div()
            .h(px(1.))
            .w_full()
            .bg(paint(palette.border))
            .into_any_element(),
    }
}

fn list_item(
    index: usize,
    ordered: bool,
    item: &ListItem,
    palette: &Palette,
    allow_remote: bool,
) -> AnyElement {
    let marker = if ordered {
        format!("{}.", index + 1)
    } else {
        "•".to_string()
    };
    div()
        .flex()
        .gap_2()
        .child(
            div()
                .w(px(28.))
                .text_color(paint(palette.dimmed))
                .child(marker),
        )
        .child(
            div().flex_1().flex().flex_col().gap_1().children(
                item.blocks
                    .iter()
                    .map(|block| render_block(block, palette, allow_remote)),
            ),
        )
        .into_any_element()
}

fn render_table(
    header: &[Vec<Block>],
    rows: &[Vec<Vec<Block>>],
    palette: &Palette,
    allow_remote: bool,
) -> AnyElement {
    let mut column = div()
        .flex()
        .flex_col()
        .border(px(1.))
        .border_color(paint(palette.border))
        .rounded(px(4.));
    if !header.is_empty() {
        column = column.child(table_row(header, palette, allow_remote, true));
    }
    for row in rows {
        column = column.child(table_row(row, palette, allow_remote, false));
    }
    column.into_any_element()
}

fn table_row(
    cells: &[Vec<Block>],
    palette: &Palette,
    allow_remote: bool,
    header: bool,
) -> AnyElement {
    let mut row = div()
        .flex()
        .border_b(px(1.))
        .border_color(paint(palette.border));
    if header {
        row = row.font_weight(FontWeight::BOLD).bg(paint(palette.surface));
    }
    // The same block renderer as the rest of the message, so a list keeps its
    // bullet or number and a quote keeps its rule.
    row.children(cells.iter().map(|cell| {
        div().flex_1().p_2().flex().flex_col().gap_2().children(
            cell.iter()
                .map(|block| render_block(block, palette, allow_remote)),
        )
    }))
    .into_any_element()
}

#[derive(Clone)]
struct Mark {
    strong: bool,
    emphasis: bool,
    code: bool,
    link: bool,
}

fn render_inlines(inlines: &[Inline], palette: &Palette, allow_remote: bool) -> Vec<AnyElement> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut ranges: Vec<(Range<usize>, Mark)> = Vec::new();
    walk_inlines(
        inlines,
        &Mark {
            strong: false,
            emphasis: false,
            code: false,
            link: false,
        },
        palette,
        allow_remote,
        &mut text,
        &mut ranges,
        &mut out,
    );
    flush_text(&mut text, &mut ranges, palette, &mut out);
    if out.is_empty() {
        out.push(div().into_any_element());
    }
    out
}

fn walk_inlines(
    inlines: &[Inline],
    mark: &Mark,
    palette: &Palette,
    allow_remote: bool,
    text: &mut String,
    ranges: &mut Vec<(Range<usize>, Mark)>,
    out: &mut Vec<AnyElement>,
) {
    for inline in inlines {
        match inline {
            Inline::Text(value) => push_text(text, ranges, value, mark),
            Inline::LineBreak => push_text(text, ranges, "\n", mark),
            Inline::Code(value) => {
                let mut code = mark.clone();
                code.code = true;
                push_text(text, ranges, value, &code);
            }
            Inline::Emphasis(children) => {
                let mut next = mark.clone();
                next.emphasis = true;
                walk_inlines(children, &next, palette, allow_remote, text, ranges, out);
            }
            Inline::Strong(children) => {
                let mut next = mark.clone();
                next.strong = true;
                walk_inlines(children, &next, palette, allow_remote, text, ranges, out);
            }
            Inline::Link { children, .. } => {
                let mut next = mark.clone();
                next.link = true;
                walk_inlines(children, &next, palette, allow_remote, text, ranges, out);
            }
            Inline::Image { source, alt } => {
                flush_text(text, ranges, palette, out);
                out.push(image_el(source, alt, palette, allow_remote));
            }
        }
    }
}

fn push_text(text: &mut String, ranges: &mut Vec<(Range<usize>, Mark)>, value: &str, mark: &Mark) {
    let start = text.len();
    text.push_str(value);
    if start != text.len() {
        ranges.push((start..text.len(), mark.clone()));
    }
}

fn flush_text(
    text: &mut String,
    ranges: &mut Vec<(Range<usize>, Mark)>,
    palette: &Palette,
    out: &mut Vec<AnyElement>,
) {
    if text.is_empty() {
        return;
    }
    let highlights = ranges.iter().filter_map(|(range, mark)| {
        let style = highlight(mark, palette)?;
        Some((range.clone(), style))
    });
    let styled = StyledText::new(SharedString::from(text.clone())).with_highlights(highlights);
    out.push(div().child(styled).into_any_element());
    text.clear();
    ranges.clear();
}

fn highlight(mark: &Mark, palette: &Palette) -> Option<HighlightStyle> {
    if !mark.strong && !mark.emphasis && !mark.code && !mark.link {
        return None;
    }
    Some(HighlightStyle {
        color: mark.link.then(|| ink(palette.accent)),
        font_weight: mark.strong.then_some(FontWeight::BOLD),
        font_style: mark.emphasis.then_some(FontStyle::Italic),
        background_color: mark.code.then(|| ink(palette.code_bg)),
        underline: mark.link.then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(ink(palette.accent)),
            wavy: false,
        }),
        strikethrough: None,
        fade_out: None,
    })
}

fn image_el(source: &ImageSource, alt: &str, palette: &Palette, allow_remote: bool) -> AnyElement {
    if let ImageSource::Data { bytes, .. } = source
        && let Some(rendered) = decode_image(bytes)
    {
        return div()
            .max_w(px(420.))
            .child(img(rendered).max_w(px(420.)).max_h(px(240.)))
            .into_any_element();
    }
    let label = match source {
        ImageSource::Remote(_) if !allow_remote => {
            if alt.is_empty() {
                "Image blocked".to_string()
            } else {
                format!("Image blocked · {alt}")
            }
        }
        ImageSource::Remote(_) => {
            if alt.is_empty() {
                "Remote image".to_string()
            } else {
                format!("Remote image · {alt}")
            }
        }
        ImageSource::Cid(cid) => format!("Inline image · {cid}"),
        ImageSource::Data { .. } => {
            if alt.is_empty() {
                "Inline image".to_string()
            } else {
                format!("Inline image · {alt}")
            }
        }
    };
    div()
        .px_3()
        .py_2()
        .border(px(1.))
        .border_color(paint(palette.border))
        .rounded(px(4.))
        .text_sm()
        .text_color(paint(palette.dimmed))
        .child(label)
        .into_any_element()
}

pub(crate) fn embedded_web(image: Arc<RenderImage>) -> AnyElement {
    div()
        .w_full()
        .child(
            img(image)
                .max_w(px(860.))
                .object_fit(gpui::ObjectFit::Contain),
        )
        .into_any_element()
}

pub(crate) fn decode_image(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    let mut data = image::load_from_memory(bytes).ok()?.into_rgba8();
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let frame = Frame::new(data);
    let frames = SmallVec::<[Frame; 1]>::from_vec(vec![frame]);
    Some(Arc::new(RenderImage::new(frames)))
}

#[cfg(test)]
mod decode_tests {
    use super::decode_image;
    use fell_render::{Block, Inline, html_to_document};

    fn find_images(inlines: &[Inline], out: &mut Vec<(String, usize)>) {
        for inline in inlines {
            match inline {
                Inline::Image { source, alt } => {
                    let n = match source {
                        fell_render::ImageSource::Data { bytes, .. } => bytes.len(),
                        _ => 0,
                    };
                    out.push((alt.clone(), n));
                    if let fell_render::ImageSource::Data { bytes, .. } = source {
                        assert!(decode_image(bytes).is_some(), "decode {alt}");
                    }
                }
                Inline::Emphasis(c) | Inline::Strong(c) | Inline::Link { children: c, .. } => {
                    find_images(c, out);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn fixture_inline_png_decodes() {
        let doc = html_to_document(include_str!(
            "../../../fell-render/fixtures/native-personal-reply.html"
        ));
        let mut found = Vec::new();
        for block in &doc.blocks {
            if let Block::Paragraph { inlines } = block {
                find_images(inlines, &mut found);
            }
        }
        assert!(
            found.iter().any(|(alt, n)| alt == "sketch" && *n > 0),
            "{found:?}"
        );
    }
}
