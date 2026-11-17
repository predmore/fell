use std::collections::HashMap;
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

/// Decoded inline images for one message.
///
/// Built once when the message is prepared. Rendering only looks up an entry,
/// and [`ImageGallery::release`] drops the GPU textures.
struct CachedImage {
    on_light: Arc<RenderImage>,
    on_dark: Arc<RenderImage>,
    width: f32,
    height: f32,
}

pub struct ImageGallery {
    decoded: HashMap<usize, CachedImage>,
    held: Vec<Arc<RenderImage>>,
}

impl ImageGallery {
    pub fn from_document(document: &StyledDocument) -> Self {
        let mut gallery = Self {
            decoded: HashMap::new(),
            held: Vec::new(),
        };
        let mut used = 0u64;
        let mut visit = |source: &ImageSource| {
            let ImageSource::Data { bytes, .. } = source else {
                return;
            };
            let key = bytes.as_ptr() as usize;
            if gallery.decoded.contains_key(&key) {
                return;
            }
            let Some(size) = image_rgba_len(bytes) else {
                return;
            };
            if !fits_message_budget(used, size) {
                return;
            }
            let Some(rgba) = decode_rgba(bytes) else {
                return;
            };
            let (on_light, on_dark, width, height) = theme_pair(&rgba);
            used += size;
            gallery.held.push(Arc::clone(&on_light));
            if !Arc::ptr_eq(&on_light, &on_dark) {
                gallery.held.push(Arc::clone(&on_dark));
            }
            gallery.decoded.insert(
                key,
                CachedImage {
                    on_light,
                    on_dark,
                    width,
                    height,
                },
            );
        };
        walk_sources(&document.blocks, &mut visit);
        gallery
    }

    pub fn get(&self, bytes: &[u8], dark_page: bool) -> Option<Arc<RenderImage>> {
        Some(Arc::clone(&self.placed(bytes, dark_page)?.0))
    }

    fn placed(&self, bytes: &[u8], dark_page: bool) -> Option<(Arc<RenderImage>, f32, f32)> {
        let cached = self.decoded.get(&(bytes.as_ptr() as usize))?;
        let image = if dark_page {
            Arc::clone(&cached.on_dark)
        } else {
            Arc::clone(&cached.on_light)
        };
        Some((image, cached.width, cached.height))
    }

    /// Remove every cached texture from the sprite atlas.
    pub fn release(&mut self, cx: &mut gpui::App) {
        for image in self.held.drain(..) {
            cx.drop_image(image, None);
        }
        self.decoded.clear();
    }
}

/// A message may not decode more raster bytes than one image at the cap.
pub(crate) fn fits_message_budget(used: u64, next: u64) -> bool {
    next > 0
        && used
            .checked_add(next)
            .is_some_and(|total| total <= MAX_IMAGE_BYTES)
}

fn image_rgba_len(bytes: &[u8]) -> Option<u64> {
    let mut header = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    header.limits(image_limits());
    let (width, height) = header.into_dimensions().ok()?;
    if !raster_allowed(width, height) {
        return None;
    }
    u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)
}

fn walk_sources(blocks: &[Block], visit: &mut impl FnMut(&ImageSource)) {
    for block in blocks {
        match block {
            Block::Heading { inlines, .. } | Block::Paragraph { inlines } => {
                walk_inlines_sources(inlines, visit);
            }
            Block::List { items, .. } => {
                for item in items {
                    walk_sources(&item.blocks, visit);
                }
            }
            Block::Quote { blocks } => walk_sources(blocks, visit),
            Block::Table { header, rows } => {
                for cell in header {
                    walk_sources(cell, visit);
                }
                for row in rows {
                    for cell in row {
                        walk_sources(cell, visit);
                    }
                }
            }
            Block::Image { source, .. } => visit(source),
            Block::Code { .. } | Block::ThematicBreak => {}
        }
    }
}

fn walk_inlines_sources(inlines: &[Inline], visit: &mut impl FnMut(&ImageSource)) {
    for inline in inlines {
        match inline {
            Inline::Emphasis(children)
            | Inline::Strong(children)
            | Inline::Link { children, .. } => {
                walk_inlines_sources(children, visit);
            }
            Inline::Image { source, .. } => visit(source),
            Inline::Text(_) | Inline::Code(_) | Inline::LineBreak => {}
        }
    }
}

/// Native styled text. Compose paints markdown through this same function.
pub fn document_column(
    document: &StyledDocument,
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
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
        column = column.child(render_block(block, palette, allow_remote, gallery));
    }
    column
}

fn render_block(
    block: &Block,
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
) -> AnyElement {
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
                .children(render_inlines(inlines, palette, allow_remote, gallery))
                .into_any_element()
        }
        Block::Paragraph { inlines } => div()
            .flex()
            .flex_col()
            .gap_2()
            .children(render_inlines(inlines, palette, allow_remote, gallery))
            .into_any_element(),
        Block::Quote { blocks } => div()
            .border_l(px(2.))
            .border_color(paint(palette.quote))
            .pl_3()
            .text_color(paint(palette.foreground))
            .flex()
            .flex_col()
            .gap_2()
            .children(
                blocks
                    .iter()
                    .map(|child| render_block(child, palette, allow_remote, gallery)),
            )
            .into_any_element(),
        Block::List { ordered, items } => div()
            .flex()
            .flex_col()
            .gap_2()
            .children(items.iter().enumerate().map(|(index, item)| {
                list_item(index, *ordered, item, palette, allow_remote, gallery)
            }))
            .into_any_element(),
        Block::Code { text } => div()
            .font_family("Liberation Mono")
            .text_sm()
            .bg(paint(palette.code_bg))
            .p_3()
            .rounded(px(4.))
            .child(text.clone())
            .into_any_element(),
        Block::Table { header, rows } => render_table(header, rows, palette, allow_remote, gallery),
        Block::Image { source, alt } => image_el(source, alt, palette, allow_remote, gallery),
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
    gallery: &ImageGallery,
) -> AnyElement {
    let marker = if ordered {
        format!("{}.", index + 1)
    } else {
        "•".to_string()
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_shrink_0()
                .text_lg()
                .text_color(paint(palette.foreground))
                .child(marker),
        )
        .child(
            div().flex_1().flex().flex_col().gap_1().children(
                item.blocks
                    .iter()
                    .map(|block| render_block(block, palette, allow_remote, gallery)),
            ),
        )
        .into_any_element()
}

fn render_table(
    header: &[Vec<Block>],
    rows: &[Vec<Vec<Block>>],
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
) -> AnyElement {
    let mut column = div()
        .flex()
        .flex_col()
        .border(px(1.))
        .border_color(paint(palette.border))
        .rounded(px(4.));
    if !header.is_empty() {
        column = column.child(table_row(header, palette, allow_remote, gallery, true));
    }
    for row in rows {
        column = column.child(table_row(row, palette, allow_remote, gallery, false));
    }
    column.into_any_element()
}

fn table_row(
    cells: &[Vec<Block>],
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
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
                .map(|block| render_block(block, palette, allow_remote, gallery)),
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

fn render_inlines(
    inlines: &[Inline],
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
) -> Vec<AnyElement> {
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
        gallery,
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

#[allow(clippy::too_many_arguments)]
fn walk_inlines(
    inlines: &[Inline],
    mark: &Mark,
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
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
                walk_inlines(
                    children,
                    &next,
                    palette,
                    allow_remote,
                    gallery,
                    text,
                    ranges,
                    out,
                );
            }
            Inline::Strong(children) => {
                let mut next = mark.clone();
                next.strong = true;
                walk_inlines(
                    children,
                    &next,
                    palette,
                    allow_remote,
                    gallery,
                    text,
                    ranges,
                    out,
                );
            }
            Inline::Link { children, .. } => {
                let mut next = mark.clone();
                next.link = true;
                walk_inlines(
                    children,
                    &next,
                    palette,
                    allow_remote,
                    gallery,
                    text,
                    ranges,
                    out,
                );
            }
            Inline::Image { source, alt } => {
                flush_text(text, ranges, palette, out);
                out.push(image_el(source, alt, palette, allow_remote, gallery));
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

fn image_el(
    source: &ImageSource,
    alt: &str,
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
) -> AnyElement {
    if let ImageSource::Data { bytes, .. } = source
        && let Some((rendered, width, height)) = gallery.placed(bytes, palette.scheme() == "dark")
    {
        let mut frame = div()
            .w(px(width))
            .h(px(height))
            .border(px(1.))
            .border_color(paint(palette.border))
            .rounded(px(4.))
            .overflow_hidden()
            .child(img(rendered).w(px(width)).h(px(height)));
        frame.style().align_self = Some(gpui::AlignItems::FlexStart);
        return frame.into_any_element();
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
    let mut placeholder = div()
        .px_2()
        .py(px(6.))
        .flex()
        .items_center()
        .gap_2()
        .border(px(1.))
        .border_color(paint(palette.border))
        .rounded(px(4.))
        .text_sm()
        .text_color(paint(palette.foreground))
        .child(picture_icon(palette))
        .child(label);
    placeholder.style().align_self = Some(gpui::AlignItems::FlexStart);
    placeholder.into_any_element()
}

pub(crate) fn embedded_web(image: Arc<RenderImage>, frame: u32) -> AnyElement {
    // A light page border has to read against both the cream email and the
    // off-white message pane. The dark border already does.
    let frame = if frame > 0x808080 { 0xb7b1a6 } else { frame };
    div()
        .w_full()
        .border(px(1.))
        .border_color(paint(frame))
        .child(
            img(image)
                .w_full()
                .max_h(px(520.))
                .object_fit(gpui::ObjectFit::Contain),
        )
        .into_any_element()
}

/// A small picture mark. The blocked-image slot used to spell "img" inside a
/// second box, which read as a placeholder glyph.
fn picture_icon(palette: &Palette) -> gpui::Div {
    div()
        .w(px(16.))
        .h(px(16.))
        .flex()
        .flex_col()
        .justify_between()
        .px(px(2.))
        .py(px(2.))
        .border(px(1.))
        .border_color(paint(palette.dimmed))
        .rounded(px(2.))
        .child(
            div().flex().justify_end().child(
                div()
                    .w(px(4.))
                    .h(px(4.))
                    .rounded(px(2.))
                    .bg(paint(palette.dimmed)),
            ),
        )
        .child(
            div()
                .w_full()
                .h(px(2.))
                .rounded(px(1.))
                .bg(paint(palette.dimmed)),
        )
}

/// Drop trailing rows that are a flat page color, so a short newsletter
/// does not keep the empty viewport under the card. A short run of that
/// color is the email's own padding and stays, so the card is not clipped.
pub(crate) fn trim_uniform_bottom(rgba: &[u8], width: u32, height: u32) -> u32 {
    const KEEP_FLAT_ROWS: usize = 16;
    let row_bytes = width as usize * 4;
    if row_bytes == 0 || height == 0 || rgba.len() < row_bytes * height as usize {
        return height;
    }
    let mut rows = height as usize;
    while rows > 1 {
        let start = (rows - 1) * row_bytes;
        if !row_is_flat(&rgba[start..start + row_bytes]) {
            break;
        }
        rows -= 1;
    }
    if rows == 1 && row_is_flat(&rgba[..row_bytes]) {
        return height;
    }
    let flat = height as usize - rows;
    (rows + flat.min(KEEP_FLAT_ROWS)) as u32
}

fn row_is_flat(row: &[u8]) -> bool {
    let Some(first) = row.get(..4) else {
        return true;
    };
    row.as_chunks::<4>().0.iter().all(|px| {
        px[0].abs_diff(first[0]) <= 2
            && px[1].abs_diff(first[1]) <= 2
            && px[2].abs_diff(first[2]) <= 2
            && px[3].abs_diff(first[3]) <= 2
    })
}

/// A mail image or a web snapshot. Far below the image crate's 512MiB default,
/// and checked before the pixels are allocated.
pub(crate) const MAX_IMAGE_EDGE: u32 = 8_192;
pub(crate) const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) fn raster_allowed(width: u32, height: u32) -> bool {
    if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return false;
    }
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .is_some_and(|bytes| bytes <= MAX_IMAGE_BYTES)
}

/// Fit a web-view snapshot under the same cap without rejecting a tall page.
///
/// Inline images still use [`raster_allowed`] as a hard stop. A newsletter
/// taller than that is scaled down so the reader can show it.
pub(crate) fn fit_snapshot(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (1, 1);
    }
    if raster_allowed(width, height) {
        return (width, height);
    }
    let (mut dst_w, mut dst_h) = if width >= height {
        let dst_w = width.min(MAX_IMAGE_EDGE);
        let dst_h = ((u64::from(height) * u64::from(dst_w)) / u64::from(width)).max(1);
        (dst_w, u32::try_from(dst_h).unwrap_or(MAX_IMAGE_EDGE))
    } else {
        let dst_h = height.min(MAX_IMAGE_EDGE);
        let dst_w = ((u64::from(width) * u64::from(dst_h)) / u64::from(height)).max(1);
        (u32::try_from(dst_w).unwrap_or(MAX_IMAGE_EDGE), dst_h)
    };
    while !raster_allowed(dst_w, dst_h) {
        if dst_w <= 1 && dst_h <= 1 {
            break;
        }
        let next_w = ((u64::from(dst_w) * 9) / 10).max(1);
        let next_h = ((u64::from(dst_h) * 9) / 10).max(1);
        if next_w == u64::from(dst_w) && next_h == u64::from(dst_h) {
            if dst_w > 1 {
                dst_w -= 1;
            } else {
                dst_h -= 1;
            }
        } else {
            dst_w = u32::try_from(next_w).unwrap_or(1);
            dst_h = u32::try_from(next_h).unwrap_or(1);
        }
    }
    (dst_w.max(1), dst_h.max(1))
}

fn image_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_IMAGE_BYTES);
    limits
}

pub(crate) fn decode_image(bytes: &[u8]) -> Option<Arc<RenderImage>> {
    decode_rgba(bytes).map(render_frame)
}

fn decode_rgba(bytes: &[u8]) -> Option<image::RgbaImage> {
    let mut header = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    header.limits(image_limits());
    let (width, height) = header.into_dimensions().ok()?;
    if !raster_allowed(width, height) {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(image_limits());
    reader.decode().ok().map(image::DynamicImage::into_rgba8)
}

/// A drawing on a flat paper color is stored twice, once per theme, from the
/// same decode. The paper becomes transparent and the strokes take the text
/// color, so a cream sketch does not sit as a light card on a dark page.
fn theme_pair(rgba: &image::RgbaImage) -> (Arc<RenderImage>, Arc<RenderImage>, f32, f32) {
    let Some(strokes) = lift_paper(rgba) else {
        let (width, height) = display_size(rgba.width(), rgba.height());
        let image = render_frame(rgba.clone());
        return (Arc::clone(&image), image, width, height);
    };
    let strokes = crop_to_marks(&strokes);
    let (width, height) = display_size(strokes.width(), strokes.height());
    (
        render_frame(tint(&strokes, [0x1a, 0x1a, 0x1a])),
        render_frame(tint(&strokes, [0xe8, 0xe8, 0xe8])),
        width,
        height,
    )
}

/// Drop the empty paper around a sketch so the frame hugs the drawing.
fn crop_to_marks(rgba: &image::RgbaImage) -> image::RgbaImage {
    let (width, height) = rgba.dimensions();
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut any = false;
    for (x, y, pixel) in rgba.enumerate_pixels() {
        if pixel[3] > 16 {
            any = true;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if !any || max_x < min_x || max_y < min_y {
        return rgba.clone();
    }
    let pad = 6;
    let x0 = min_x.saturating_sub(pad);
    let y0 = min_y.saturating_sub(pad);
    let x1 = (max_x + pad).min(width - 1);
    let y1 = (max_y + pad).min(height - 1);
    image::imageops::crop_imm(rgba, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image()
}

fn display_size(width: u32, height: u32) -> (f32, f32) {
    let width = width.max(1) as f32;
    let height = height.max(1) as f32;
    let scale = (220. / width).min(120. / height).min(1.);
    (width * scale, height * scale)
}

fn lift_paper(rgba: &image::RgbaImage) -> Option<image::RgbaImage> {
    let pixels: Vec<[u8; 4]> = rgba.pixels().map(|p| p.0).collect();
    let opaque: Vec<_> = pixels.iter().filter(|p| p[3] >= 16).copied().collect();
    if opaque.is_empty() {
        return None;
    }
    let mut counts: HashMap<(u8, u8, u8), usize> = HashMap::new();
    for px in &opaque {
        let key = (px[0] & !7, px[1] & !7, px[2] & !7);
        *counts.entry(key).or_default() += 1;
    }
    let (&paper, &count) = counts.iter().max_by_key(|(_, n)| *n)?;
    if count * 100 / opaque.len() < 80 {
        return None;
    }
    let mut marks = Vec::new();
    for px in &opaque {
        let key = (px[0] & !7, px[1] & !7, px[2] & !7);
        let paper_pixel = key.0.abs_diff(paper.0) <= 8
            && key.1.abs_diff(paper.1) <= 8
            && key.2.abs_diff(paper.2) <= 8;
        if !paper_pixel {
            marks.push(*px);
        }
    }
    if marks.is_empty() || marks.iter().any(|px| !near_grayscale(px[0], px[1], px[2])) {
        return None;
    }
    let mut out = rgba.clone();
    for px in out.pixels_mut() {
        let key = (px[0] & !7, px[1] & !7, px[2] & !7);
        let paper_pixel = px[3] < 16
            || (key.0.abs_diff(paper.0) <= 8
                && key.1.abs_diff(paper.1) <= 8
                && key.2.abs_diff(paper.2) <= 8);
        if paper_pixel {
            *px = image::Rgba([0, 0, 0, 0]);
        } else {
            *px = image::Rgba([255, 255, 255, 255]);
        }
    }
    Some(out)
}

fn near_grayscale(red: u8, green: u8, blue: u8) -> bool {
    red.max(green).max(blue) - red.min(green).min(blue) <= 24
}

fn tint(src: &image::RgbaImage, rgb: [u8; 3]) -> image::RgbaImage {
    let mut out = src.clone();
    for px in out.pixels_mut() {
        if px[3] > 0 {
            *px = image::Rgba([rgb[0], rgb[1], rgb[2], px[3]]);
        }
    }
    out
}

fn render_frame(mut data: image::RgbaImage) -> Arc<RenderImage> {
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let frame = Frame::new(data);
    let frames = SmallVec::<[Frame; 1]>::from_vec(vec![frame]);
    Arc::new(RenderImage::new(frames))
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
    fn colored_mark_on_white_stays_colored() {
        use super::lift_paper;
        let mut image = image::RgbaImage::new(4, 4);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgba([250, 250, 250, 255]);
        }
        image.put_pixel(1, 1, image::Rgba([200, 16, 16, 255]));
        assert!(
            lift_paper(&image).is_none(),
            "a red mark is not a grayscale sketch"
        );
        assert_eq!(image.get_pixel(1, 1).0, [200, 16, 16, 255]);
    }

    #[test]
    fn sketch_frame_hugs_the_drawing() {
        use super::theme_pair;
        let doc = html_to_document(include_str!(
            "../../../fell-render/fixtures/native-personal-reply.html"
        ));
        let bytes = find_data(&doc.blocks).expect("fixture has a data image");
        let rgba = super::decode_rgba(bytes).expect("png");
        let (_light, _dark, width, height) = theme_pair(&rgba);
        assert!(width < 160. && height < 90., "{width}x{height}");
        assert!(width > 40. && height > 20., "{width}x{height}");
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

    #[test]
    fn oversized_image_is_not_decoded() {
        use super::{MAX_IMAGE_EDGE, raster_allowed};

        assert!(raster_allowed(160, 90));
        assert!(!raster_allowed(100_000, 100_000));
        assert!(!raster_allowed(MAX_IMAGE_EDGE, MAX_IMAGE_EDGE));
        assert!(!raster_allowed(u32::MAX, u32::MAX));
        assert!(decode_image(&png_header(100_000, 100_000)).is_none());
    }

    #[test]
    fn tall_snapshot_is_scaled_into_the_cap() {
        use super::{fit_snapshot, raster_allowed};

        assert_eq!(fit_snapshot(880, 1_200), (880, 1_200));
        let (width, height) = fit_snapshot(880, 20_000);
        assert!(raster_allowed(width, height), "{width}x{height}");
        assert!(height > width, "{width}x{height}");
        let (width, height) = fit_snapshot(880, 9_500);
        assert!(raster_allowed(width, height), "{width}x{height}");
        assert!(height > 4_000, "{width}x{height}");
    }

    #[test]
    fn uniform_page_color_under_a_card_is_trimmed() {
        use super::trim_uniform_bottom;
        let width = 4u32;
        let height = 40u32;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for y in 0..10 {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                rgba[i] = 20;
                rgba[i + 1] = x as u8;
                rgba[i + 2] = 40;
                rgba[i + 3] = 255;
            }
        }
        for y in 10..40 {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                rgba[i] = 231;
                rgba[i + 1] = 226;
                rgba[i + 2] = 216;
                rgba[i + 3] = 255;
            }
        }
        assert_eq!(trim_uniform_bottom(&rgba, width, height), 26);
        let short = &rgba[..width as usize * 18 * 4];
        assert_eq!(trim_uniform_bottom(short, width, 18), 18);
    }

    #[test]
    fn inline_images_are_decoded_once_under_a_message_cap() {
        use super::{ImageGallery, fits_message_budget};
        use fell_render::html_to_document;

        assert!(fits_message_budget(0, 64));
        assert!(!fits_message_budget(32 * 1024 * 1024, 1));
        assert!(!fits_message_budget(u64::MAX - 1, 2));

        let doc = html_to_document(include_str!(
            "../../../fell-render/fixtures/native-personal-reply.html"
        ));
        let gallery = ImageGallery::from_document(&doc);
        assert_eq!(gallery.decoded.len(), 1, "the sketch decodes once");
        let bytes = match find_data(&doc.blocks) {
            Some(bytes) => bytes,
            None => panic!("fixture has a data image"),
        };
        let on_light = gallery.get(bytes, false).expect("cached");
        let on_dark = gallery.get(bytes, true).expect("cached");
        assert!(
            gallery
                .held
                .iter()
                .any(|image| std::sync::Arc::ptr_eq(image, &on_light))
        );
        assert!(
            gallery
                .held
                .iter()
                .any(|image| std::sync::Arc::ptr_eq(image, &on_dark))
        );
        assert!(!std::sync::Arc::ptr_eq(&on_light, &on_dark));
    }

    fn find_data(blocks: &[fell_render::Block]) -> Option<&[u8]> {
        for block in blocks {
            match block {
                fell_render::Block::Paragraph { inlines }
                | fell_render::Block::Heading { inlines, .. } => {
                    if let Some(bytes) = find_data_inline(inlines) {
                        return Some(bytes);
                    }
                }
                fell_render::Block::Quote { blocks } => {
                    if let Some(bytes) = find_data(blocks) {
                        return Some(bytes);
                    }
                }
                fell_render::Block::List { items, .. } => {
                    for item in items {
                        if let Some(bytes) = find_data(&item.blocks) {
                            return Some(bytes);
                        }
                    }
                }
                fell_render::Block::Image {
                    source: fell_render::ImageSource::Data { bytes, .. },
                    ..
                } => return Some(bytes),
                _ => {}
            }
        }
        None
    }

    fn find_data_inline(inlines: &[fell_render::Inline]) -> Option<&[u8]> {
        for inline in inlines {
            match inline {
                fell_render::Inline::Image {
                    source: fell_render::ImageSource::Data { bytes, .. },
                    ..
                } => return Some(bytes),
                fell_render::Inline::Emphasis(children)
                | fell_render::Inline::Strong(children)
                | fell_render::Inline::Link { children, .. } => {
                    if let Some(bytes) = find_data_inline(children) {
                        return Some(bytes);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        fn crc(data: &[u8]) -> u32 {
            let mut sum = 0xFFFF_FFFFu32;
            for &byte in data {
                sum ^= u32::from(byte);
                for _ in 0..8 {
                    let bit = sum & 1;
                    sum >>= 1;
                    if bit == 1 {
                        sum ^= 0xEDB8_8320;
                    }
                }
            }
            !sum
        }
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut body = Vec::with_capacity(4 + data.len());
            body.extend_from_slice(kind);
            body.extend_from_slice(data);
            let sum = crc(&body);
            let mut out = Vec::new();
            out.extend_from_slice(&(u32::try_from(data.len()).unwrap_or(u32::MAX)).to_be_bytes());
            out.extend_from_slice(&body);
            out.extend_from_slice(&sum.to_be_bytes());
            out
        }
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        png.extend(chunk(b"IHDR", &ihdr));
        png.extend(chunk(b"IEND", &[]));
        png
    }
}
