use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use fell_render::{Align, Block, ImageSource, Inline, ListItem, ListLabelStyle, StyledDocument};
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, FontStyle, FontWeight, GlobalElementId,
    HighlightStyle, InspectorElementId, IntoElement, LayoutId, Pixels, RenderImage, SharedString,
    TextAlign, UnderlineStyle, Window, div, img, prelude::*, px,
};
use image::Frame;
use smallvec::SmallVec;
use unicode_linebreak::{BreakOpportunity, linebreaks};
use unicode_segmentation::UnicodeSegmentation;

use super::theme::{Palette, ink, paint};

mod links;
use links::{LinkControls, LinkSpan, checked_target, link_controls, link_spans};

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

#[derive(Default)]
pub struct ImageGallery {
    decoded: HashMap<usize, CachedImage>,
    held: Vec<Arc<RenderImage>>,
}

impl ImageGallery {
    pub fn from_document(document: &StyledDocument) -> Self {
        let mut gallery = Self::default();
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

    /// Every decoded texture, so an owner that cannot take `&mut` (a shared
    /// cache entry) can still remove them from the sprite atlas.
    pub fn images(&self) -> impl Iterator<Item = &Arc<RenderImage>> {
        self.held.iter()
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
            Block::Quote { blocks } | Block::Small { blocks } | Block::ReplyHeader { blocks } => {
                walk_sources(blocks, visit)
            }
            Block::Table { header, rows, .. } => {
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
            | Inline::Strike(children)
            | Inline::Link { children, .. } => {
                walk_inlines_sources(children, visit);
            }
            Inline::Image { source, .. } => visit(source),
            Inline::Text(_) | Inline::Code(_) | Inline::LineBreak => {}
        }
    }
}

/// Space between blocks. A short letter uses this rhythm instead of a
/// closing rule.
pub const BLOCK_GAP: f32 = 24.;

/// A caption under a sketch uses half the paragraph rhythm, so it reads as
/// a label for the drawing. The next block still gets [`BLOCK_GAP`].
const CAPTION_GAP: f32 = BLOCK_GAP / 2.;

/// Native styled text. Compose paints markdown through this same function.
pub fn document_column(
    document: &StyledDocument,
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
    width: f32,
    markers: &ListMarkerFont,
) -> gpui::Div {
    if document.is_empty() {
        return blocks_column(&[], palette, allow_remote, gallery, width, markers).child(
            div()
                .text_color(paint(palette.dimmed))
                .child("This message has no body."),
        );
    }
    blocks_column(
        &document.blocks,
        palette,
        allow_remote,
        gallery,
        width,
        markers,
    )
}

/// A run of a document's blocks, as [`document_column`] paints them. The
/// conversation paints a message's sections one at a time through this, so
/// folded history keeps the same type and rhythm when it opens.
pub fn blocks_column(
    blocks: &[Block],
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
    width: f32,
    markers: &ListMarkerFont,
) -> gpui::Div {
    let markers = markers.with_top_level_lists(blocks);
    blocks_column_spaced(
        blocks,
        palette,
        allow_remote,
        gallery,
        BlockColumn::full(width),
        BLOCK_GAP,
        &markers,
    )
}

/// The wide block width and, when set, the position of the prose within it.
#[derive(Clone, Copy)]
pub(crate) struct BlockColumn {
    width: f32,
    prose: Option<(f32, f32)>,
    wide_inset: f32,
    reader: Option<ReaderTable>,
    quote_color: Option<u32>,
    list_depth: usize,
    links: bool,
}

#[derive(Clone, Copy)]
struct ReaderTable {
    reading_wide: f32,
    progress: f32,
    fill: u32,
    card_right: f32,
    pane_right: f32,
}

impl BlockColumn {
    pub fn full(width: f32) -> Self {
        Self {
            width,
            prose: None,
            wide_inset: 0.0,
            reader: None,
            quote_color: None,
            list_depth: 0,
            links: true,
        }
    }

    pub fn reading(
        (wide, wide_inset): (f32, f32),
        (prose, prose_inset): (f32, f32),
        (reading_wide, progress): (f32, f32),
        (card_right, pane_right): (f32, f32),
        fill: u32,
    ) -> Self {
        Self {
            width: wide,
            prose: Some((prose, prose_inset)),
            wide_inset,
            reader: Some(ReaderTable {
                reading_wide,
                progress,
                fill,
                card_right,
                pane_right,
            }),
            quote_color: None,
            list_depth: 0,
            links: true,
        }
    }

    fn with_links(self, links: bool) -> Self {
        Self { links, ..self }
    }

    fn indented(self, indent: f32, rtl: bool) -> Self {
        match self.prose {
            Some((width, inset)) => Self {
                prose: Some((
                    (width - indent).max(0.0),
                    if rtl { inset } else { inset + indent },
                )),
                ..self
            },
            None => Self {
                width: (self.width - indent).max(0.0),
                ..self
            },
        }
    }

    fn list_marker_width(self, local: f32, top_level: f32) -> f32 {
        if self.list_depth == 0 {
            local.max(top_level)
        } else {
            local
        }
    }

    fn prose(self, element: gpui::Div) -> gpui::Div {
        match (self.prose, self.reader) {
            (Some((width, inset)), Some(reader)) => {
                crate::view::disclosure::clip_column(inset, width, reader.card_right, element)
            }
            _ => element,
        }
    }

    fn wide(self, element: gpui::Div) -> gpui::Div {
        match self.reader {
            Some(reader) => crate::view::disclosure::clip_column(
                self.wide_inset,
                self.width,
                reader.pane_right,
                element,
            ),
            None => element,
        }
    }

    /// Indent the prose under the rule. Wide children stay in their column.
    fn history(self, indent: f32, rule: f32, rtl: bool) -> (Self, Option<f32>) {
        let edge = self.prose.map(|(width, inset)| {
            if rtl {
                inset + (width - rule).max(0.)
            } else {
                inset
            }
        });
        (self.indented(indent, rtl), edge)
    }

    pub fn quote(
        self,
        element: gpui::Div,
        indent: f32,
        rule: f32,
        color: u32,
        rtl: bool,
    ) -> (gpui::Div, Self) {
        let (mut layout, edge) = self.history(indent, rule, rtl);
        layout.quote_color = Some(color);
        (quote_rule(element, edge, indent, rule, color, rtl), layout)
    }

    fn table(self, intrinsic: f32, widths: &[ColumnWidth]) -> TableLayout {
        let (Some(reader), Some((prose_width, prose_inset))) = (self.reader, self.prose) else {
            return TableLayout {
                width: self.width,
                inset: 0.0,
                right: self.width,
                grid: table_columns_fit(widths, self.width),
            };
        };
        let width = intrinsic.min(self.width);
        if width > READING_MEASURE_PX && table_columns_fit(widths, width) {
            let reading_width = intrinsic.min(reader.reading_wide);
            TableLayout {
                width,
                inset: self.wide_inset
                    + (reader.reading_wide - reading_width) * 0.5 * reader.progress,
                right: reader.pane_right,
                grid: true,
            }
        } else {
            let width = intrinsic.min(prose_width);
            let grid = table_columns_fit(widths, width);
            TableLayout {
                width: if grid { width } else { prose_width },
                inset: prose_inset,
                right: reader.card_right,
                grid,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TableLayout {
    width: f32,
    inset: f32,
    right: f32,
    grid: bool,
}

fn quote_rule(
    element: gpui::Div,
    edge: Option<f32>,
    indent: f32,
    rule: f32,
    color: u32,
    rtl: bool,
) -> gpui::Div {
    if let Some(edge) = edge {
        element.relative().child(
            div()
                .absolute()
                .left(px(edge))
                .top_0()
                .bottom_0()
                .w(px(rule))
                .bg(paint(color)),
        )
    } else if rtl {
        element
            .pr(px(indent - rule))
            .border_r(px(rule))
            .border_color(paint(color))
    } else {
        element
            .pl(px(indent - rule))
            .border_l(px(rule))
            .border_color(paint(color))
    }
}

#[derive(Clone, Copy)]
pub(crate) enum BlockColumns {
    Fixed(BlockColumn),
    Reflow([BlockColumn; 2], crate::motion::ReadingReflow),
}

impl From<BlockColumn> for BlockColumns {
    fn from(layout: BlockColumn) -> Self {
        Self::Fixed(layout)
    }
}

impl BlockColumns {
    pub fn with_links(self, links: bool) -> Self {
        match self {
            Self::Fixed(layout) => Self::Fixed(layout.with_links(links)),
            Self::Reflow(layouts, frame) => {
                Self::Reflow(layouts.map(|layout| layout.with_links(links)), frame)
            }
        }
    }

    pub fn reflow(
        split: BlockColumn,
        reading: BlockColumn,
        frame: crate::motion::ReadingReflow,
    ) -> Self {
        Self::Reflow([split, reading], frame)
    }

    pub fn quote(
        self,
        element: gpui::Div,
        indent: f32,
        rule: f32,
        color: u32,
        rtl: bool,
    ) -> (gpui::Div, Self) {
        match self {
            Self::Fixed(layout) => {
                let (element, layout) = layout.quote(element, indent, rule, color, rtl);
                (element, layout.into())
            }
            Self::Reflow([split, reading], frame) => {
                let (mut split, split_edge) = split.history(indent, rule, rtl);
                let (mut reading, reading_edge) = reading.history(indent, rule, rtl);
                split.quote_color = Some(color);
                reading.quote_color = Some(color);
                let edge = split_edge
                    .zip(reading_edge)
                    .map(|(split, reading)| crate::motion::lerp(split, reading, frame.height));
                (
                    quote_rule(element, edge, indent, rule, color, rtl),
                    Self::Reflow([split, reading], frame),
                )
            }
        }
    }

    fn draw(
        self,
        blocks: &[Block],
        mut render: impl FnMut(BlockColumn) -> AnyElement,
    ) -> AnyElement {
        match self {
            Self::Fixed(layout) => render(layout),
            Self::Reflow([split, reading], frame) => {
                let same_prose = split.prose == reading.prose;
                if same_prose
                    && (split.width == reading.width || !blocks.iter().any(uses_wide_column))
                {
                    render(reading)
                } else {
                    let width = reading
                        .reader
                        .map_or(reading.width, |reader| reader.pane_right);
                    let reading_live = frame.alpha[1] > frame.alpha[0];
                    crate::view::disclosure::reflow(
                        width,
                        frame,
                        [
                            render(split.with_links(split.links && !reading_live)),
                            render(reading.with_links(reading.links && reading_live)),
                        ],
                    )
                }
            }
        }
    }
}

fn uses_wide_column(block: &Block) -> bool {
    fn has_image(inline: &Inline) -> bool {
        match inline {
            Inline::Image { .. } => true,
            Inline::Strong(children)
            | Inline::Emphasis(children)
            | Inline::Strike(children)
            | Inline::Link { children, .. } => children.iter().any(has_image),
            _ => false,
        }
    }
    match block {
        Block::Code { .. } | Block::Table { .. } | Block::Image { .. } => true,
        Block::Heading { inlines, .. } | Block::Paragraph { inlines } => {
            inlines.iter().any(has_image)
        }
        Block::Quote { blocks } | Block::Small { blocks } | Block::ReplyHeader { blocks } => {
            blocks.iter().any(uses_wide_column)
        }
        Block::List { items, .. } => items
            .iter()
            .flat_map(|item| &item.blocks)
            .any(uses_wide_column),
        Block::ThematicBreak => false,
    }
}

/// A comfortable line of mail text, about 72 characters.
pub(crate) const READING_MEASURE_PX: f32 = 600.0;

/// The reader's font before the parent element applies it during layout.
#[derive(Clone)]
pub struct ListMarkerFont<'a> {
    window: &'a Window,
    style: gpui::TextStyle,
    top_level_width: f32,
}

impl<'a> ListMarkerFont<'a> {
    pub fn new(window: &'a Window, mut style: gpui::TextStyle) -> Self {
        style.font_features = crate::view::tabular_figures();
        Self {
            window,
            style,
            top_level_width: LIST_MIN_MARKER_PX,
        }
    }

    pub(crate) fn with_top_level_lists<'b>(
        &self,
        blocks: impl IntoIterator<Item = &'b Block>,
    ) -> Self {
        Self {
            top_level_width: widest_top_level_marker(blocks, |label| self.width(label)),
            ..self.clone()
        }
    }

    fn with_size(&self, font_size: Pixels) -> Self {
        let mut style = self.style.clone();
        style.font_size = font_size.into();
        Self {
            style,
            ..self.clone()
        }
    }

    fn size(&self) -> Pixels {
        self.style.font_size.to_pixels(self.window.rem_size())
    }

    fn width(&self, label: &SharedString) -> f32 {
        let run = self.style.to_run(label.len());
        self.window
            .text_system()
            .shape_line(label.clone(), self.size(), &[run], None)
            .width
            .into()
    }
}

#[derive(Clone, Copy)]
struct BlockPaint<'a> {
    palette: &'a Palette,
    allow_remote: bool,
    gallery: &'a ImageGallery,
    markers: &'a ListMarkerFont<'a>,
}

/// [`blocks_column`] with `gap` between blocks instead of [`BLOCK_GAP`]:
/// opened history sits tighter than the sender's own paragraphs.
pub(crate) fn blocks_column_spaced(
    blocks: &[Block],
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
    layout: impl Into<BlockColumns>,
    gap: f32,
    markers: &ListMarkerFont,
) -> gpui::Div {
    blocks_column_with_headings(
        blocks,
        palette,
        allow_remote,
        gallery,
        layout,
        (gap, None),
        markers,
    )
}

pub(crate) fn blocks_column_with_headings(
    blocks: &[Block],
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
    layout: impl Into<BlockColumns>,
    (gap, continuation): (f32, Option<&crate::scroll_anchor::Continuation>),
    markers: &ListMarkerFont,
) -> gpui::Div {
    // A short letter should reach the lower part of the pane. The gaps and
    // line height do that without adding a fake closing rule.
    let mut column = div().flex().flex_col().line_height(px(22.));
    let layout = layout.into();
    let context = BlockPaint {
        palette,
        allow_remote,
        gallery,
        markers,
    };
    let mut previous: Option<&Block> = None;
    for (start, end) in column_runs(blocks, allow_remote, gallery) {
        let run = &blocks[start..end];
        let child = if let [
            Block::Table {
                header,
                rows,
                align,
            },
        ] = run
        {
            render_table(header, rows, align, context, layout)
        } else {
            layout.draw(run, |layout| {
                if end - start == 2 {
                    sketch_with_caption(&blocks[start], &blocks[start + 1], context, layout)
                } else {
                    render_block(&blocks[start], context, layout)
                }
            })
        };
        let gap = previous.map_or(0., |above| {
            // Blocked-image lines stacked one under another read as a
            // list of what is missing, a line apart.
            if blocked_line(above, allow_remote, gallery)
                && blocked_line(&blocks[start], allow_remote, gallery)
            {
                STACKED_LINE_GAP
            } else {
                gap_between(above, &blocks[start]) * gap
            }
        });
        let child = if let Some(continuation) = continuation
            && matches!(blocks[start], Block::Heading { .. })
        {
            continuation.heading(child).into_any_element()
        } else {
            child
        };
        column = column.child(div().mt(px(gap)).child(child));
        previous = Some(&blocks[end - 1]);
    }
    column
}

/// Between two stacked blocked-image lines: the line height does the rest.
const STACKED_LINE_GAP: f32 = 2.;

/// A block that paints as one dimmed `Image blocked` line.
fn blocked_line(block: &Block, allow_remote: bool, gallery: &ImageGallery) -> bool {
    placeholder_caption(block, allow_remote, gallery)
        || matches!(block, Block::Paragraph { inlines } if blocked_image_line(inlines, allow_remote).is_some())
}

/// Space above `block`, as a share of the column's gap. A heading sits
/// closer to the text it introduces than to the text before it.
fn gap_between(above: &Block, block: &Block) -> f32 {
    match (above, block) {
        (_, Block::Heading { .. }) => 1.5,
        (Block::Heading { .. }, _) => 0.5,
        _ => 1.0,
    }
}

fn sketch_with_caption(
    sketch: &Block,
    caption: &Block,
    context: BlockPaint,
    layout: BlockColumn,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(CAPTION_GAP))
        .child(render_block(sketch, context, layout))
        .child(render_block(caption, context, layout))
        .into_any_element()
}

/// Inclusive-exclusive ranges of blocks that share one column child.
fn column_runs(
    blocks: &[Block],
    allow_remote: bool,
    gallery: &ImageGallery,
) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut index = 0;
    while index < blocks.len() {
        if caption_under_sketch(&blocks[index], blocks.get(index + 1), allow_remote, gallery) {
            runs.push((index, index + 2));
            index += 2;
        } else {
            runs.push((index, index + 1));
            index += 1;
        }
    }
    runs
}

fn caption_under_sketch(
    block: &Block,
    next: Option<&Block>,
    allow_remote: bool,
    gallery: &ImageGallery,
) -> bool {
    let Some(next) = next else {
        return false;
    };
    decoded_sketch(block, gallery) && placeholder_caption(next, allow_remote, gallery)
}

fn sole_image(block: &Block) -> Option<&ImageSource> {
    match block {
        Block::Image { source, .. } => Some(source),
        Block::Paragraph { inlines } => match inlines.as_slice() {
            [Inline::Image { source, .. }] => Some(source),
            _ => None,
        },
        _ => None,
    }
}

fn decoded_sketch(block: &Block, gallery: &ImageGallery) -> bool {
    let Some(ImageSource::Data { bytes, .. }) = sole_image(block) else {
        return false;
    };
    gallery.placed(bytes, false).is_some()
}

/// The gray line for a blocked remote image, a missing cid, or data that
/// did not decode. An allowed remote image is not that blocked line, so it
/// keeps the paragraph gap.
fn placeholder_caption(block: &Block, allow_remote: bool, gallery: &ImageGallery) -> bool {
    sole_image(block).is_some_and(|source| paints_placeholder(source, allow_remote, gallery))
}

fn paints_placeholder(source: &ImageSource, allow_remote: bool, gallery: &ImageGallery) -> bool {
    match source {
        ImageSource::Data { bytes, .. } => gallery.placed(bytes, false).is_none(),
        ImageSource::Remote(_) => !allow_remote,
        ImageSource::Cid(_) => true,
    }
}

fn render_block(block: &Block, context: BlockPaint, layout: BlockColumn) -> AnyElement {
    let BlockPaint {
        palette,
        allow_remote,
        gallery,
        markers,
    } = context;
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
                .children(render_inlines(
                    inlines,
                    palette,
                    allow_remote,
                    gallery,
                    layout,
                ))
                .into_any_element()
        }
        Block::Paragraph { inlines } => div()
            .flex()
            .flex_col()
            .gap_2()
            .children(render_inlines(
                inlines,
                palette,
                allow_remote,
                gallery,
                layout,
            ))
            .into_any_element(),
        Block::Small { blocks } => {
            let quiet = Palette {
                foreground: palette.dimmed,
                ..*palette
            };
            let small = markers.with_size(px(FINE_PRINT_PX));
            let context = BlockPaint {
                palette: &quiet,
                markers: &small,
                ..context
            };
            div()
                .text_size(px(FINE_PRINT_PX))
                .line_height(px(18.))
                .text_color(paint(palette.dimmed))
                .flex()
                .flex_col()
                .gap(px(BLOCK_GAP / 2.))
                .children(
                    blocks
                        .iter()
                        .map(|child| render_block(child, context, layout)),
                )
                .into_any_element()
        }
        Block::ReplyHeader { blocks } => div()
            .flex()
            .flex_col()
            .gap(px(BLOCK_GAP))
            .children(
                blocks
                    .iter()
                    .map(|child| render_block(child, context, layout)),
            )
            .into_any_element(),
        Block::Quote { blocks } => {
            let (quote, layout) =
                layout.quote(div(), QUOTE_INSET_PX, QUOTE_RULE_PX, palette.quote, false);
            quote
                .text_color(paint(palette.foreground))
                .flex()
                .flex_col()
                .gap_2()
                .children(
                    blocks
                        .iter()
                        .map(|child| render_block(child, context, layout)),
                )
                .into_any_element()
        }
        Block::List {
            ordered,
            start,
            style,
            items,
        } => {
            let labels: Vec<_> =
                list_labels(*ordered, *start, *style, items.len(), layout.list_depth).collect();
            let distinct_labels = if *ordered {
                labels.as_slice()
            } else {
                &labels[..labels.len().min(1)]
            };
            let local_width =
                marker_width(distinct_labels.iter().map(|label| markers.width(label)));
            let marker_width = layout.list_marker_width(local_width, markers.top_level_width);
            div()
                .flex()
                .flex_col()
                .gap_2()
                .children(
                    items
                        .iter()
                        .zip(labels)
                        .map(|(item, label)| list_item(item, label, marker_width, context, layout)),
                )
                .into_any_element()
        }
        Block::Code { text, large: false } => layout
            .wide(
                div().child(
                    div()
                        .font_family(CODE_FONT)
                        .bg(paint(palette.code_bg))
                        .px_3()
                        .py_2()
                        .rounded(px(4.))
                        .child(PreText::new(text)),
                ),
            )
            .into_any_element(),
        // A box that fits the code, not the line: it reads as one thing.
        Block::Code { text, large: true } => layout
            .wide(
                div().flex().child(
                    div()
                        .flex_none()
                        .when(layout.prose.is_some(), |code| code.max_w_full())
                        .font_family(CODE_FONT)
                        .bg(paint(palette.code_bg))
                        .px_3()
                        .py_2()
                        .rounded(px(4.))
                        .child(PreText::large(text)),
                ),
            )
            .into_any_element(),
        Block::Table {
            header,
            rows,
            align,
        } => render_table(header, rows, align, context, layout.into()),
        Block::Image { source, alt } => {
            image_el(source, alt, palette, allow_remote, gallery, layout)
        }
        Block::ThematicBreak => layout
            .prose(div().h(px(1.)).w_full().bg(paint(palette.border)))
            .into_any_element(),
    }
}

const QUOTE_RULE_PX: f32 = 3.;
const QUOTE_INSET_PX: f32 = QUOTE_RULE_PX + 16.;
const LIST_MARKER_GAP_PX: f32 = 6.;
const LIST_MIN_MARKER_PX: f32 = 18.;
const LIST_BULLETS: [&str; 3] = ["•", "◦", "▪"];

fn list_labels(
    ordered: bool,
    start: u64,
    style: ListLabelStyle,
    len: usize,
    depth: usize,
) -> impl Iterator<Item = SharedString> {
    (0..len).map(move |index| {
        if ordered {
            style.label(start.saturating_add(index as u64)).into()
        } else {
            LIST_BULLETS[depth % LIST_BULLETS.len()].into()
        }
    })
}

fn marker_width(widths: impl Iterator<Item = f32>) -> f32 {
    widths.fold(LIST_MIN_MARKER_PX, f32::max).ceil()
}

fn widest_top_level_marker<'a>(
    blocks: impl IntoIterator<Item = &'a Block>,
    mut measure: impl FnMut(&SharedString) -> f32,
) -> f32 {
    let labels = blocks.into_iter().filter_map(|block| {
        let Block::List {
            ordered,
            start,
            style,
            items,
        } = block
        else {
            return None;
        };
        let len = if *ordered {
            items.len()
        } else {
            items.len().min(1)
        };
        Some(list_labels(*ordered, *start, *style, len, 0))
    });
    marker_width(labels.flatten().map(|label| measure(&label)))
}

fn list_item(
    item: &ListItem,
    label: SharedString,
    marker_width: f32,
    context: BlockPaint,
    layout: BlockColumn,
) -> AnyElement {
    let nested = BlockColumn {
        list_depth: layout.list_depth + 1,
        ..layout.indented(marker_width + LIST_MARKER_GAP_PX, false)
    };
    // Top-aligned with the same line height, so the marker sits on the
    // first line's baseline however many lines the item wraps to.
    div()
        .relative()
        .flex()
        .items_start()
        .gap(px(LIST_MARKER_GAP_PX))
        .child(
            div()
                .w(px(marker_width))
                .when_some(layout.prose, |marker, (_, inset)| {
                    marker.absolute().top_0().left(px(inset))
                })
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_right()
                .font(context.markers.style.font())
                .text_size(context.markers.size())
                .text_color(paint(context.palette.foreground))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_2()
                .children(
                    item.blocks
                        .iter()
                        .map(|block| render_block(block, context, nested)),
                ),
        )
        .into_any_element()
}

fn render_table(
    header: &[Vec<Block>],
    rows: &[Vec<Vec<Block>>],
    align: &[Align],
    context: BlockPaint,
    columns: BlockColumns,
) -> AnyElement {
    let make = |layout: BlockColumn| {
        let cell = |blocks: &Vec<Block>| {
            let mut lines = Vec::new();
            push_cell_lines(blocks, &mut lines);
            let content = div()
                .flex()
                .flex_col()
                .gap_2()
                .children(
                    blocks
                        .iter()
                        .map(|block| render_block(block, context, BlockColumn::full(layout.width))),
                )
                .into_any_element();
            TableCell {
                content,
                lines,
                bold: blocks_are_bold(blocks),
            }
        };
        TableElement {
            header: header.iter().map(cell).collect(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(cell).collect())
                .collect(),
            align: align.to_vec(),
            palette: *context.palette,
            layout,
            alternate: None,
            inner: None,
        }
    };
    match columns {
        BlockColumns::Fixed(layout) => make(layout),
        BlockColumns::Reflow([split, reading], frame) => {
            let mut table = make(split);
            table.alternate = Some((Box::new(make(reading)), frame));
            table
        }
    }
    .into_any_element()
}

struct TableCell {
    content: AnyElement,
    lines: Vec<String>,
    bold: bool,
}

/// Measure with the inherited body face before laying out the rows.
struct TableElement {
    header: Vec<TableCell>,
    rows: Vec<Vec<TableCell>>,
    align: Vec<Align>,
    palette: Palette,
    layout: BlockColumn,
    alternate: Option<(Box<TableElement>, crate::motion::ReadingReflow)>,
    inner: Option<AnyElement>,
}

impl IntoElement for TableElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TableElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let mut bold_style = style.clone();
        bold_style.font_weight = FontWeight::BOLD;
        let text_system = window.text_system();
        let measure = |text: &str, strong: bool| -> f32 {
            let style = if strong { &bold_style } else { &style };
            text.split('\n')
                .map(|line| {
                    let run = style.to_run(line.len());
                    f32::from(
                        text_system
                            .shape_line(line.to_string().into(), font_size, &[run], None)
                            .width,
                    )
                })
                .fold(0., f32::max)
        };
        let lines: Vec<MeasuredRow<'_>> = std::iter::once(&self.header)
            .chain(&self.rows)
            .enumerate()
            .map(|(row, cells)| {
                (
                    row == 0,
                    cells
                        .iter()
                        .map(|cell| (cell.lines.as_slice(), row == 0 || cell.bold))
                        .collect(),
                )
            })
            .collect();
        let widths = column_widths(&lines, measure);
        let intrinsic = widths.iter().map(|column| column.max).sum::<f32>() + 2. * TABLE_BORDER_PX;
        let mut layout = self.layout.table(intrinsic, &widths);
        let mut alternate = self.alternate.take().map(|(table, frame)| {
            let layout = table.layout.table(intrinsic, &widths);
            (table, frame, layout)
        });
        let labels: Vec<_> = if self.rows.is_empty()
            || layout.grid && alternate.as_ref().is_none_or(|(_, _, layout)| layout.grid)
        {
            Vec::new()
        } else {
            (0..widths.len())
                .map(|column| {
                    let label = self
                        .header
                        .get(column)
                        .map_or_else(String::new, |cell| cell.lines.join(" "));
                    if label.trim().is_empty() {
                        format!("Column {}", column + 1)
                    } else {
                        label
                    }
                })
                .collect()
        };
        let label_width = labels
            .iter()
            .map(|label| measure(label, true))
            .fold(0., f32::max)
            + 2.;
        let fit_stacked = |layout: &mut TableLayout| {
            if layout.grid || self.layout.reader.is_none() {
                return;
            }
            let longest = widths.iter().map(|column| column.max).fold(0.0, f32::max);
            let natural = longest
                + 2. * TABLE_BORDER_PX
                + if labels.is_empty() {
                    0.0
                } else {
                    label_width + 2. * TABLE_CELL_PAD_PX
                };
            layout.width = layout.width.min(natural);
        };
        fit_stacked(&mut layout);
        if let Some((_, _, alternate)) = &mut alternate {
            fit_stacked(alternate);
        }
        let width = layout.width;
        let palette = self.palette;
        let table = div()
            .flex()
            .flex_col()
            .overflow_hidden()
            .border(px(TABLE_BORDER_PX))
            .border_color(paint(
                self.layout.quote_color.map_or(palette.border, |quote| {
                    crate::motion::mix(palette.border, quote, 0.65)
                }),
            ))
            .rounded(px(4.))
            .when_some(self.layout.reader, |table, reader| {
                table.bg(paint(reader.fill))
            });
        let header = std::mem::take(&mut self.header);
        let rows = std::mem::take(&mut self.rows);
        let table = if layout.grid {
            self.grid(table, header, rows, &widths)
        } else {
            self.stacked(table, header, rows, &widths, width, (&labels, label_width))
        };
        let table = if self.layout.reader.is_some() {
            crate::view::disclosure::clip_column(layout.inset, width, layout.right, table)
        } else {
            table
        };
        let table = table.into_any_element();
        let mut inner = match alternate {
            Some((alternate, frame, other)) if other != layout => {
                let pane_right = self
                    .layout
                    .reader
                    .map_or(layout.right, |reader| reader.pane_right);
                crate::view::disclosure::reflow(
                    pane_right,
                    frame,
                    [table, (*alternate).into_any_element()],
                )
            }
            _ => table,
        };
        let layout = inner.request_layout(window, cx);
        self.inner = Some(inner);
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if let Some(inner) = self.inner.as_mut() {
            inner.prepaint(window, cx);
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(inner) = self.inner.as_mut() {
            inner.paint(window, cx);
        }
    }
}

impl TableElement {
    fn grid(
        &self,
        mut table: gpui::Div,
        header: Vec<TableCell>,
        rows: Vec<Vec<TableCell>>,
        widths: &[ColumnWidth],
    ) -> gpui::Div {
        for (index, row) in std::iter::once(header).chain(rows).enumerate() {
            if index == 0 && row.is_empty() {
                continue;
            }
            let mut cells = row.into_iter();
            let mut line = self.row().w_full();
            if index == 0 {
                line = line
                    .font_weight(FontWeight::BOLD)
                    .bg(paint(self.palette.surface));
            }
            for (column, width) in widths.iter().enumerate() {
                let mut slot = div()
                    .flex_basis(px(width.basis))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .px(px(TABLE_CELL_PAD_PX / 2.))
                    .py_2()
                    .flex()
                    .flex_col();
                slot.style().flex_grow = Some(width.max - width.basis + 1.0);
                slot = match self.align.get(column).copied().unwrap_or_default() {
                    Align::Start => slot,
                    Align::Center => slot.text_align(TextAlign::Center),
                    Align::End => slot.text_align(TextAlign::Right),
                };
                line = line.child(slot.children(cells.next().map(|cell| cell.content)));
            }
            table = table.child(line);
        }
        table
    }

    fn stacked(
        &self,
        table: gpui::Div,
        header: Vec<TableCell>,
        rows: Vec<Vec<TableCell>>,
        widths: &[ColumnWidth],
        width: f32,
        (labels, label_width): (&[String], f32),
    ) -> gpui::Div {
        if rows.is_empty() {
            return table.child(
                self.row()
                    .flex_col()
                    .gap_2()
                    .p_2()
                    .font_weight(FontWeight::BOLD)
                    .children(header.into_iter().map(|cell| cell.content)),
            );
        }
        let room = (width - 2. * TABLE_BORDER_PX - TABLE_CELL_PAD_PX).max(0.);
        // Share a label column across rows while it leaves room for the
        // values. At very small widths a label sits over its own value.
        let value_basis = widths.iter().map(|width| width.basis).fold(0., f32::max);
        let beside = label_width + TABLE_CELL_PAD_PX + value_basis <= room;
        table.children(rows.into_iter().map(|row| {
            let mut cells = row.into_iter();
            self.row()
                .flex_col()
                .gap(px(4.))
                .px(px(TABLE_CELL_PAD_PX / 2.))
                .py_2()
                .children(labels.iter().map(|label| {
                    let label = div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(paint(self.palette.dimmed))
                        .child(label.clone())
                        .when(beside, |label| label.w(px(label_width)).flex_none());
                    let value = div()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .when(beside, |value| value.flex_1())
                        .children(cells.next().map(|cell| cell.content));
                    div()
                        .flex()
                        .when_else(
                            beside,
                            |field| field.gap(px(TABLE_CELL_PAD_PX)),
                            |field| field.flex_col(),
                        )
                        .child(label)
                        .child(value)
                }))
        }))
    }

    fn row(&self) -> gpui::Div {
        div()
            .flex()
            .border_b(px(1.))
            .border_color(paint(self.palette.border))
    }
}

fn table_columns_fit(widths: &[ColumnWidth], width: f32) -> bool {
    widths.iter().map(|column| column.basis).sum::<f32>() + 2. * TABLE_BORDER_PX <= width
}

const TABLE_BORDER_PX: f32 = 1.;

/// A row to measure: whether it is the header, then each cell's lines and
/// whether the cell is set bold.
type MeasuredRow<'a> = (bool, Vec<(&'a [String], bool)>);

/// A column's widths, padding included.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnWidth {
    /// Its longest word: narrower would break a word.
    pub min: f32,
    /// Its longest line unwrapped: wider is empty space.
    pub max: f32,
    /// What the row gives it first: its longest word, or for a column of
    /// short values (prices, counts) its longest value, so a number never
    /// wraps while a header over it may.
    pub basis: f32,
}

/// Cell padding, both sides together.
const TABLE_CELL_PAD_PX: f32 = 16.;

/// No column asks for more than this as its narrowest, so one long URL
/// cannot push the table past the pane.
const TABLE_MAX_MIN_PX: f32 = 180.;

/// A column whose body cells all fit in this keeps them on one line.
const TABLE_SHORT_PX: f32 = 140.;

/// Each column's widths from its cells' lines (`rows` holds whether each
/// row is the header, then each cell's lines and whether it is set bold;
/// `measure` takes text and that flag). The row gives every column
/// its basis and shares what is left in proportion to `max - basis`, as a
/// browser's auto table layout shares the room past min-content.
fn column_widths(
    rows: &[MeasuredRow<'_>],
    measure: impl Fn(&str, bool) -> f32,
) -> Vec<ColumnWidth> {
    let columns = rows.iter().map(|(_, cells)| cells.len()).max().unwrap_or(0);
    let mut min = vec![0f32; columns];
    let mut max = vec![0f32; columns];
    let mut body_max = vec![0f32; columns];
    for (header, cells) in rows {
        for (column, (lines, bold)) in cells.iter().enumerate() {
            let word = lines
                .iter()
                .flat_map(|line| line.split_whitespace())
                .map(|word| measure(word, *bold))
                .fold(0.0, f32::max);
            let line = lines
                .iter()
                .map(|line| measure(line.trim(), *bold))
                .fold(0.0, f32::max);
            min[column] = min[column].max(word.min(TABLE_MAX_MIN_PX));
            max[column] = max[column].max(line);
            if !header {
                body_max[column] = body_max[column].max(line);
            }
        }
    }
    // A little slack, so rounding never wraps a word that fits.
    const SLACK: f32 = 2.;
    (0..columns)
        .map(|column| {
            let min = min[column] + TABLE_CELL_PAD_PX + SLACK;
            let max = (max[column] + TABLE_CELL_PAD_PX + SLACK).max(min);
            let short = body_max[column] + TABLE_CELL_PAD_PX + SLACK;
            let basis = if body_max[column] <= TABLE_SHORT_PX {
                min.max(short)
            } else {
                min
            };
            ColumnWidth { min, max, basis }
        })
        .collect()
}

/// Whether a cell has bold text in it, so it is measured in the bold face.
fn blocks_are_bold(blocks: &[Block]) -> bool {
    fn inlines(runs: &[Inline]) -> bool {
        runs.iter().any(|inline| match inline {
            Inline::Strong(_) => true,
            Inline::Emphasis(children)
            | Inline::Strike(children)
            | Inline::Link { children, .. } => inlines(children),
            _ => false,
        })
    }
    blocks.iter().any(|block| match block {
        Block::Heading { .. } => true,
        Block::Paragraph { inlines: runs } => inlines(runs),
        Block::Quote { blocks } | Block::Small { blocks } | Block::ReplyHeader { blocks } => {
            blocks_are_bold(blocks)
        }
        Block::List { items, .. } => items.iter().any(|item| blocks_are_bold(&item.blocks)),
        _ => false,
    })
}

/// A cell's lines of text: one per paragraph line, list item or code line.
fn push_cell_lines(blocks: &[Block], out: &mut Vec<String>) {
    fn inlines(runs: &[Inline], out: &mut Vec<String>) {
        for inline in runs {
            match inline {
                Inline::Text(text) | Inline::Code(text) => {
                    if out.is_empty() {
                        out.push(String::new());
                    }
                    out.last_mut().expect("a line").push_str(text);
                }
                Inline::Emphasis(children)
                | Inline::Strong(children)
                | Inline::Strike(children)
                | Inline::Link { children, .. } => inlines(children, out),
                Inline::Image { alt, .. } => {
                    if out.is_empty() {
                        out.push(String::new());
                    }
                    out.last_mut().expect("a line").push_str(alt);
                }
                Inline::LineBreak => out.push(String::new()),
            }
        }
    }
    for block in blocks {
        match block {
            Block::Heading { inlines: text, .. } | Block::Paragraph { inlines: text } => {
                let mut lines = Vec::new();
                inlines(text, &mut lines);
                out.extend(lines);
            }
            Block::List { items, .. } => {
                for item in items {
                    let mut lines = Vec::new();
                    push_cell_lines(&item.blocks, &mut lines);
                    // The marker column.
                    out.extend(
                        lines
                            .into_iter()
                            .map(|line| format!("\u{2003}\u{2003}{line}")),
                    );
                }
            }
            Block::Quote { blocks } | Block::Small { blocks } | Block::ReplyHeader { blocks } => {
                push_cell_lines(blocks, out)
            }
            Block::Code { text, .. } => out.extend(text.lines().map(str::to_string)),
            Block::Image { alt, .. } => out.push(format!("Image blocked \u{b7} {alt}")),
            Block::Table { .. } | Block::ThematicBreak => {}
        }
    }
}

#[derive(Clone, Default)]
struct Mark {
    strong: bool,
    emphasis: bool,
    strike: bool,
    /// On the code tint: the code and the padding either side of it.
    code: bool,
    /// In the monospace face: the code itself.
    mono: bool,
    link: Option<links::LinkMark>,
}

/// Fine print's size: a step under the body, as the sender meant it.
const FINE_PRINT_PX: f32 = 12.5;

/// The face inline code and code blocks use.
const CODE_FONT: &str = "Liberation Mono";

/// About 2 px either side of inline code, inside its tint. No-break, so
/// the padding never starts or ends a line on its own.
const CODE_PAD: &str = "\u{202F}";

fn render_inlines(
    inlines: &[Inline],
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
    layout: BlockColumn,
) -> Vec<AnyElement> {
    if let Some(line) = blocked_image_line(inlines, allow_remote) {
        return vec![
            layout
                .prose(div().child(dimmed_line(line, palette)))
                .into_any_element(),
        ];
    }
    let mut out = Vec::new();
    let mut text = String::new();
    let mut ranges: Vec<(Range<usize>, Mark)> = Vec::new();
    let plain = Mark::default();
    let source = inlines.as_ptr() as usize;
    let mut at = 0;
    while at < inlines.len() {
        // Blocked images side by side in a sentence are one line too.
        if !allow_remote
            && let Some((end, alts)) = image_run(inlines, at)
            && alts.len() >= 2
        {
            flush_text(&mut text, &mut ranges, palette, layout, source, &mut out);
            out.push(
                layout
                    .prose(div().child(blocked_label(&alts, palette)))
                    .into_any_element(),
            );
            at = end;
            continue;
        }
        walk_inlines(
            &inlines[at..at + 1],
            &plain,
            palette,
            allow_remote,
            gallery,
            layout,
            source,
            &mut text,
            &mut ranges,
            &mut out,
        );
        at += 1;
    }
    flush_text(&mut text, &mut ranges, palette, layout, source, &mut out);
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
    layout: BlockColumn,
    source: usize,
    text: &mut String,
    ranges: &mut Vec<(Range<usize>, Mark)>,
    out: &mut Vec<AnyElement>,
) {
    for inline in inlines {
        match inline {
            Inline::Text(value) => push_text(text, ranges, value, mark),
            Inline::LineBreak => push_text(text, ranges, "\n", mark),
            Inline::Code(value) => {
                let mut pad = mark.clone();
                pad.code = true;
                let mut code = pad.clone();
                code.mono = true;
                push_text(text, ranges, CODE_PAD, &pad);
                push_text(text, ranges, value, &code);
                push_text(text, ranges, CODE_PAD, &pad);
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
                    layout,
                    source,
                    text,
                    ranges,
                    out,
                );
            }
            Inline::Strike(children) => {
                let mut next = mark.clone();
                next.strike = true;
                walk_inlines(
                    children,
                    &next,
                    palette,
                    allow_remote,
                    gallery,
                    layout,
                    source,
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
                    layout,
                    source,
                    text,
                    ranges,
                    out,
                );
            }
            Inline::Link { href, children } => {
                let mut next = mark.clone();
                next.link = checked_target(href).map(|target| links::LinkMark {
                    source: inline as *const Inline as usize,
                    target,
                });
                walk_inlines(
                    children,
                    &next,
                    palette,
                    allow_remote,
                    gallery,
                    layout,
                    source,
                    text,
                    ranges,
                    out,
                );
            }
            Inline::Image { source: image, alt } => {
                flush_text(text, ranges, palette, layout, source, out);
                out.push(image_el(image, alt, palette, allow_remote, gallery, layout));
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
    layout: BlockColumn,
    source: usize,
    out: &mut Vec<AnyElement>,
) {
    if text.is_empty() {
        return;
    }
    let runs: Vec<(usize, Mark)> = ranges
        .drain(..)
        .map(|(range, mark)| (range.len(), mark))
        .collect();
    let mut element = MailText::new(SharedString::from(std::mem::take(text)), runs, *palette);
    if layout.links && !element.links.is_empty() {
        // The source inlines belong to the cached document. Images can
        // split the same paragraph into several separate text elements.
        element.id = Some(ElementId::Name(
            format!("mail-text-{source}-{}", out.len()).into(),
        ));
    }
    out.push(
        layout
            .prose(div().w_full().child(element))
            .into_any_element(),
    );
}

pub(crate) fn plain_text(
    text: &crate::text_render::PlainText,
    palette: Palette,
    block: usize,
    live: bool,
) -> impl IntoElement {
    let mut runs = Vec::new();
    let mut at = 0;
    for link in text.links.iter() {
        let range = &link.range;
        if at < range.start {
            runs.push((range.start - at, Mark::default()));
        }
        at = range.end;
        runs.push((
            range.len(),
            Mark {
                link: Some(links::LinkMark {
                    source: range.start,
                    target: link.target.clone(),
                }),
                ..Mark::default()
            },
        ));
    }
    if at < text.len() {
        runs.push((text.len() - at, Mark::default()));
    }
    let mut element = MailText::new(text.text.clone(), runs, palette);
    if live && !element.links.is_empty() {
        element.id = Some(ElementId::named_usize("plain-mail-text", block));
    }
    div().w_full().child(element)
}

/// One line of text shaped with its runs. `shape_text`, not `shape_line`:
/// gpui 0.2.2's `shape_line` joins a run whose font changes (bold, code)
/// into the one before it when the colour and decorations match, so a bold
/// label at the start of a line set the whole line bold.
fn shape_one(
    window: &Window,
    text: &str,
    font_size: Pixels,
    runs: &[gpui::TextRun],
) -> gpui::WrappedLine {
    window
        .text_system()
        .shape_text(
            SharedString::from(text.to_string()),
            font_size,
            runs,
            None,
            None,
        )
        .ok()
        .and_then(|lines| lines.into_iter().next())
        .unwrap_or_default()
}

/// The x of each byte of a shaped left-to-right line, as `x_for_index`
/// gives it: where the first glyph at or after the byte starts. The text
/// being wrapped has soft hyphens that the shaper's input omits. Mixed
/// direction glyph order can make byte offsets unusable.
fn byte_xs(glyphs: impl Iterator<Item = (usize, f32)>, text: &str, width: f32) -> Option<Vec<f32>> {
    let mut hyphens = text.match_indices(SOFT_HYPHEN).peekable();
    let mut skipped = 0;
    let glyphs: Vec<_> = glyphs
        .map(|(index, x)| {
            while hyphens.next_if(|(at, _)| *at <= index + skipped).is_some() {
                skipped += SOFT_HYPHEN.len_utf8();
            }
            (index + skipped, x)
        })
        .collect();
    if !glyphs.iter().map(|(index, _)| index).is_sorted() {
        return None;
    }
    let len = text.len();
    let mut xs = vec![width; len + 1];
    let mut next = glyphs.len();
    let mut x = width;
    for at in (0..=len).rev() {
        while next > 0 && glyphs[next - 1].0 >= at {
            next -= 1;
            x = glyphs[next].1;
        }
        xs[at] = x;
    }
    Some(xs)
}

/// The byte ranges of inline code (with its padding), which a line breaks
/// inside only when it is wider than the line.
fn code_ranges(runs: &[(usize, Mark)]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut at = 0;
    for (len, mark) in runs {
        if mark.code {
            match out.last_mut() {
                Some(last) if last.end == at => last.end = at + len,
                _ => out.push(at..at + len),
            }
        }
        at += len;
    }
    out
}

/// Set right to left: the first letter is of a right-to-left script
/// (Hebrew, Arabic, Syriac, Thaana), as `dir="auto"` decides it.
pub fn is_right_to_left(text: &str) -> bool {
    text.chars()
        .find(|ch| ch.is_alphabetic())
        .is_some_and(|ch| matches!(ch as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF))
}

/// The runs of `runs` (covering the text in order) that fall in `range`,
/// cut to it.
fn slice_runs(runs: &[gpui::TextRun], range: &Range<usize>) -> Vec<gpui::TextRun> {
    let mut out = Vec::new();
    let mut start = 0;
    for run in runs {
        let end = start + run.len;
        let from = start.max(range.start);
        let to = end.min(range.end);
        if from < to {
            let mut cut = run.clone();
            cut.len = to - from;
            out.push(cut);
        }
        start = end;
    }
    out
}

/// Byte ranges of each line of `text` broken to fit `width`,
/// from `measure` of a range's width. Hard line breaks are kept.
#[cfg(test)]
pub(crate) fn wrap_words(
    text: &str,
    width: Option<f32>,
    mut measure: impl FnMut(Range<usize>) -> f32,
) -> Vec<Range<usize>> {
    wrap_mail(text, width, &[], &[], |range, _| measure(range))
        .into_iter()
        .map(|line| line.range)
        .collect()
}

const SOFT_HYPHEN: char = '\u{ad}';

struct MailLine {
    range: Range<usize>,
    hyphen: bool,
}

/// Fit lines at Unicode line-break opportunities using the measured width.
/// Inline code stays together when it fits. An unbreakable span wider than
/// the entire line falls back to grapheme boundaries.
fn wrap_mail(
    text: &str,
    width: Option<f32>,
    keep: &[Range<usize>],
    urls: &[Range<usize>],
    mut measure: impl FnMut(Range<usize>, bool) -> f32,
) -> Vec<MailLine> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for hard in text.split('\n') {
        let base = offset;
        offset += hard.len() + 1;
        let Some(width) = width.filter(|_| !hard.is_empty()) else {
            lines.push(MailLine {
                range: base..base + hard.len(),
                hyphen: false,
            });
            continue;
        };
        let end = base + hard.len();
        // Suppress soft breaks strictly inside code that fits on a line.
        let held: Vec<&Range<usize>> = keep
            .iter()
            .filter(|range| range.start < end && range.end > base)
            .filter(|range| measure((*range).clone(), false) <= width)
            .collect();
        let mut breaks: Vec<_> = linebreaks(hard).collect();
        prefer_url_breaks(hard, base, urls, &mut breaks);
        // Treat an inline code span after a space as an indivisible item.
        // Its leading punctuation (for example, "/" in a path) would
        // otherwise suppress the break before the span.
        for range in &held {
            if let Some(before) = hard.get(..range.start.saturating_sub(base))
                && before.ends_with([' ', '\t'])
            {
                let at = range.start - base;
                if let Err(index) = breaks.binary_search_by_key(&at, |&(next, _)| next) {
                    breaks.insert(index, (at, BreakOpportunity::Allowed));
                }
            }
        }
        let mut line_start = base;
        let mut line_end = base;
        let mut next_start = base;
        let mut line_hyphen = false;
        for (after, opportunity) in breaks {
            let mandatory = opportunity == BreakOpportunity::Mandatory;
            let next = base + after;
            if !mandatory
                && held
                    .iter()
                    .any(|range| range.start < next && next < range.end)
            {
                continue;
            }
            let content = &hard[..after];
            let content = if mandatory {
                content.trim_end_matches([
                    '\r', '\u{000b}', '\u{000c}', '\u{0085}', '\u{2028}', '\u{2029}',
                ])
            } else {
                content
            };
            let word_end = (base + content.trim_end_matches([' ', '\t']).len()).max(line_start);
            let hyphen = !mandatory && word_end == next && content.ends_with(SOFT_HYPHEN);
            let mut overflows = measure(line_start..word_end, false) > width;
            if overflows && line_end > line_start {
                lines.push(MailLine {
                    range: line_start..line_end,
                    hyphen: line_hyphen,
                });
                line_start = next_start;
                overflows = measure(line_start..word_end, false) > width;
            }
            if overflows {
                while line_start < word_end {
                    let mut cut = line_start;
                    let mut cut_hyphen = false;
                    for (at, grapheme) in text[line_start..word_end].grapheme_indices(true) {
                        let next = line_start + at + grapheme.len();
                        if cut > line_start && measure(line_start..next, false) > width {
                            break;
                        }
                        let hyphen_here = next < word_end && grapheme.ends_with(SOFT_HYPHEN);
                        if hyphen_here
                            && cut > line_start
                            && measure(line_start..next, true) > width
                        {
                            continue;
                        }
                        cut = next;
                        cut_hyphen = hyphen_here && cut > line_start + grapheme.len();
                    }
                    if cut >= word_end {
                        break;
                    }
                    lines.push(MailLine {
                        range: line_start..cut,
                        hyphen: cut_hyphen,
                    });
                    line_start = cut;
                }
            }
            if mandatory {
                lines.push(MailLine {
                    range: line_start..word_end,
                    hyphen: false,
                });
                line_start = next;
                line_end = next;
            } else if !hyphen || measure(line_start..word_end, true) <= width {
                line_end = word_end;
                next_start = next;
                line_hyphen = hyphen;
            }
        }
    }
    lines
}

fn prefer_url_breaks(
    hard: &str,
    base: usize,
    urls: &[Range<usize>],
    breaks: &mut Vec<(usize, BreakOpportunity)>,
) {
    let end = base + hard.len();
    let from = urls.partition_point(|url| url.end <= base);
    let to = urls.partition_point(|url| url.start < end);
    let Some(urls) = urls.get(from..to).filter(|urls| !urls.is_empty()) else {
        return;
    };
    // URL separators take precedence over ordinary breaks inside the URL.
    // Oversized segments can still break at grapheme boundaries.
    breaks.retain(|&(at, opportunity)| {
        let next = base + at;
        let index = urls.partition_point(|url| url.end <= next);
        opportunity == BreakOpportunity::Mandatory
            || urls.get(index).is_none_or(|url| next <= url.start)
    });
    for url in urls {
        let start = url.start.saturating_sub(base);
        let stop = url.end.min(end) - base;
        let label = &hard[start..stop];
        let scheme_end = label.find("://").map_or(0, |at| at + 3);
        breaks.extend(
            label[scheme_end..]
                .match_indices(['/', '?', '&'])
                .map(|(at, _)| start + scheme_end + at + 1)
                .filter(|&at| at < stop)
                .map(|at| (at, BreakOpportunity::Allowed)),
        );
    }
    breaks.sort_unstable_by_key(|&(at, _)| at);
}

/// Remove discretionary hyphens from the shaper's input, keeping a visible
/// hyphen and its style only where a line actually breaks at one.
fn visible_mail_line<'a>(
    text: &'a str,
    range: &Range<usize>,
    hyphen: bool,
) -> std::borrow::Cow<'a, str> {
    let text = &text[range.clone()];
    if !text.contains(SOFT_HYPHEN) {
        return text.into();
    }
    let mut visible = text.replace(SOFT_HYPHEN, "");
    if hyphen {
        visible.push('-');
    }
    visible.into()
}

fn shape_mail_line(
    window: &Window,
    text: &str,
    font_size: Pixels,
    runs: &[gpui::TextRun],
    range: &Range<usize>,
    hyphen: bool,
    focused: Option<&Range<usize>>,
) -> gpui::WrappedLine {
    let visible = visible_mail_line(text, range, hyphen);
    let mut runs = slice_runs(runs, range);
    if let Some(focused) = focused
        && focused.start < range.end
        && range.start < focused.end
    {
        // Link spans follow run boundaries. Keep the glyphs and their wrap
        // while the focus outline takes over from the underline.
        let mut at = range.start;
        for run in &mut runs {
            if focused.contains(&at) {
                run.underline = None;
            }
            at += run.len;
        }
    }
    if matches!(visible, std::borrow::Cow::Owned(_)) {
        let mut remaining = &text[range.clone()];
        let count = runs.len();
        for (index, run) in runs.iter_mut().enumerate() {
            let (part, rest) = remaining.split_at(run.len);
            run.len -= part.matches(SOFT_HYPHEN).count() * SOFT_HYPHEN.len_utf8();
            if hyphen && index + 1 == count {
                run.len += 1;
            }
            remaining = rest;
        }
        runs.retain(|run| run.len > 0);
    }
    shape_one(window, &visible, font_size, &runs)
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct WrapKey {
    text: SharedString,
    fonts: Vec<(usize, gpui::Font)>,
    keep: Vec<Range<usize>>,
    urls: Vec<Range<usize>>,
    size: Pixels,
    width: Option<Pixels>,
}

#[derive(Default)]
struct WrappedMail {
    lines: Vec<MailLine>,
    width: Pixels,
}

/// MailText elements are rebuilt by their parent on animation frames. Keep
/// their line breaks across frames, with a bounded cache shared by the window
/// layouts. Color and decorations do not affect the line breaks.
#[derive(Default)]
struct WrapCache {
    entries: HashMap<Rc<WrapKey>, Rc<WrappedMail>>,
    order: VecDeque<Rc<WrapKey>>,
    bytes: usize,
}

#[derive(Default)]
struct MailWrapping(RefCell<WrapCache>);

impl gpui::Global for MailWrapping {}

impl WrapCache {
    fn get_or_wrap(
        &mut self,
        key: WrapKey,
        wrap: impl FnOnce(&WrapKey) -> WrappedMail,
    ) -> Rc<WrappedMail> {
        const MAX_ENTRIES: usize = 4096;
        const MAX_BYTES: usize = 8 * 1024 * 1024;
        if let Some(wrapped) = self.entries.get(&key) {
            return wrapped.clone();
        }
        let wrapped = Rc::new(wrap(&key));
        let size = |key: &WrapKey, body: &WrappedMail| {
            key.text.len()
                + body.lines.len() * std::mem::size_of::<MailLine>()
                + key.fonts.len() * std::mem::size_of::<(usize, gpui::Font)>()
                + (key.keep.len() + key.urls.len()) * std::mem::size_of::<Range<usize>>()
        };
        let bytes = size(&key, &wrapped);
        if bytes <= MAX_BYTES {
            while self.bytes + bytes > MAX_BYTES || self.entries.len() >= MAX_ENTRIES {
                let oldest = self.order.pop_front().expect("a full cache has entries");
                let body = self
                    .entries
                    .remove(&oldest)
                    .expect("the queued entry is cached");
                self.bytes -= size(&oldest, &body);
            }
            let key = Rc::new(key);
            self.order.push_back(key.clone());
            self.entries.insert(key, wrapped.clone());
            self.bytes += bytes;
        }
        wrapped
    }
}

/// Text in the inherited face with runs of bold, italic, links and inline
/// code. Lines wrap here to keep inline code together when it fits and so a
/// link's underline ends with its last letter on each line instead of
/// running into the next line's indent. A paragraph written right to left
/// is set flush right (gpui would wrap it out of place). Inline code needs
/// its own face, which a highlight cannot set, so the runs are built from
/// the text style the element lays out in.
struct MailText {
    id: Option<ElementId>,
    text: SharedString,
    /// Byte length and marks of each run, covering the text in order.
    runs: Vec<(usize, Mark)>,
    /// Inline code, kept on one line when it fits.
    keep: Vec<Range<usize>>,
    links: Vec<LinkSpan>,
    rtl: bool,
    palette: Palette,
    wrapped: Rc<RefCell<Rc<WrappedMail>>>,
    line_height: Pixels,
    font_size: Pixels,
    text_runs: Vec<gpui::TextRun>,
}

impl MailText {
    fn new(text: SharedString, runs: Vec<(usize, Mark)>, palette: Palette) -> Self {
        let links = link_spans(&runs);
        Self {
            id: None,
            rtl: is_right_to_left(&text),
            keep: code_ranges(&runs),
            links,
            text,
            runs,
            palette,
            wrapped: Rc::default(),
            line_height: px(0.),
            font_size: px(0.),
            text_runs: Vec::new(),
        }
    }
}

impl IntoElement for MailText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MailText {
    type RequestLayoutState = ();
    type PrepaintState = LinkControls;

    fn id(&self) -> Option<ElementId> {
        self.id.clone()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        if !cx.has_global::<MailWrapping>() {
            cx.set_global(MailWrapping::default());
        }
        let base = window.text_style();
        let font_size = base.font_size.to_pixels(window.rem_size());
        let line_height = base
            .line_height
            .to_pixels(font_size.into(), window.rem_size());
        self.line_height = line_height;
        self.font_size = font_size;
        self.text_runs = self
            .runs
            .iter()
            .map(|(len, mark)| {
                let mut style = base.clone();
                if let Some(highlight) = highlight(mark, &self.palette) {
                    style = style.highlight(highlight);
                }
                if mark.mono {
                    style.font_family = CODE_FONT.into();
                }
                style.to_run(*len)
            })
            .collect();
        let text = self.text.clone();
        let runs = self.text_runs.clone();
        let fonts: Vec<_> = runs.iter().map(|run| (run.len, run.font.clone())).collect();
        let keep = self.keep.clone();
        let urls: Vec<_> = self
            .links
            .iter()
            .filter(|link| text.get(link.range.clone()) == Some(&link.target))
            .map(|link| link.range.clone())
            .collect();
        let rtl = self.rtl;
        let measured = Rc::clone(&self.wrapped);
        let layout = window.request_measured_layout(
            gpui::Style::default(),
            move |known, available, window, cx| {
                let width = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let key = WrapKey {
                    text: text.clone(),
                    fonts: fonts.clone(),
                    keep: keep.clone(),
                    urls: urls.clone(),
                    size: font_size,
                    width,
                };
                let wrapped = cx
                    .global::<MailWrapping>()
                    .0
                    .borrow_mut()
                    .get_or_wrap(key, |key| wrap_shaped_mail(key, &runs, rtl, window));
                let size = gpui::size(wrapped.width, line_height * wrapped.lines.len() as f32);
                *measured.borrow_mut() = wrapped;
                size
            },
        );
        (layout, ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        id.map_or_else(LinkControls::default, |id| {
            window.with_element_state(id, |state, window| {
                let mut state = state.unwrap_or_default();
                let controls = link_controls(self, bounds, &mut state, window, cx);
                (controls, state)
            })
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        controls: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for control in &mut controls.elements {
            control.paint(window, cx);
        }
        self.for_visible_line(
            bounds,
            window,
            controls.focused.as_ref(),
            |_, shaped, origin, window| {
                let align = TextAlign::Left;
                let _ = shaped.paint_background(origin, self.line_height, align, None, window, cx);
                let _ = shaped.paint(origin, self.line_height, align, None, window, cx);
            },
        );
    }
}

impl MailText {
    fn for_visible_line(
        &self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        focused: Option<&Range<usize>>,
        mut visit: impl FnMut(&MailLine, gpui::WrappedLine, gpui::Point<Pixels>, &mut Window),
    ) {
        let mask = window.content_mask().bounds;
        let height = f32::from(self.line_height).max(1.0);
        let first = (f32::from(mask.top() - bounds.top()) / height)
            .floor()
            .max(1.0) as usize
            - 1;
        let end = (f32::from(mask.bottom() - bounds.top()) / height)
            .ceil()
            .max(0.0) as usize
            + 1;
        let wrapped = self.wrapped.borrow();
        for (at, line) in wrapped.lines.iter().enumerate().take(end).skip(first) {
            if line.range.is_empty() {
                continue;
            }
            let shaped = shape_mail_line(
                window,
                &self.text,
                self.font_size,
                &self.text_runs,
                &line.range,
                line.hyphen,
                focused,
            );
            let x = if self.rtl {
                bounds.right() - shaped.unwrapped_layout.width
            } else {
                bounds.left()
            };
            let origin = gpui::point(x, bounds.origin.y + self.line_height * at as f32);
            visit(line, shaped, origin, window);
        }
    }
}

fn wrap_shaped_mail(
    key: &WrapKey,
    runs: &[gpui::TextRun],
    rtl: bool,
    window: &Window,
) -> WrappedMail {
    let shape = |range: &Range<usize>, hyphen| {
        shape_mail_line(window, &key.text, key.size, runs, range, hyphen, None)
    };
    let mut run_ends = None;
    let mut advances = HashMap::new();
    wrap_measured_mail(
        key,
        rtl,
        |range| {
            let shaped = shape(range, false);
            let layout = &shaped.unwrapped_layout;
            let glyphs = layout
                .runs
                .iter()
                .flat_map(|run| &run.glyphs)
                .map(|glyph| (glyph.index, f32::from(glyph.position.x)));
            byte_xs(glyphs, &key.text[range.clone()], f32::from(layout.width))
        },
        |range, hyphen| f32::from(shape(range, hyphen).unwrapped_layout.width),
        |end| {
            let ends = run_ends.get_or_insert_with(|| {
                runs.iter()
                    .scan(0, |end, run| {
                        *end += run.len;
                        Some(*end)
                    })
                    .collect::<Vec<_>>()
            });
            // Use the font of the last source byte, the same one that
            // shape_mail_line uses to draw the discretionary hyphen.
            let run = &runs[ends.partition_point(|&run_end| run_end < end)];
            *advances.entry(&run.font).or_insert_with(|| {
                let run = gpui::TextRun {
                    len: 1,
                    ..run.clone()
                };
                f32::from(
                    shape_one(window, "-", key.size, &[run])
                        .unwrapped_layout
                        .width,
                )
            })
        },
    )
}

fn wrap_measured_mail(
    key: &WrapKey,
    rtl: bool,
    mut prefix_xs: impl FnMut(&Range<usize>) -> Option<Vec<f32>>,
    mut measure: impl FnMut(&Range<usize>, bool) -> f32,
    mut hyphen_advance: impl FnMut(usize) -> f32,
) -> WrappedMail {
    // In byte order, a range's width is the distance between its ends
    // on a hard line, shaped once. Reordered glyphs need range shaping.
    let mut hard = Vec::new();
    if key.width.is_some() && !rtl {
        let mut at = 0;
        for line in key.text.split('\n') {
            let xs = prefix_xs(&(at..at + line.len()));
            hard.push((at, xs));
            at += line.len() + 1;
        }
    }
    let lines = wrap_mail(
        &key.text,
        key.width.map(f32::from),
        &key.keep,
        &key.urls,
        |range, hyphen| {
            let line = hard.partition_point(|(start, _)| *start <= range.start);
            match line.checked_sub(1).map(|line| &hard[line]) {
                Some((start, Some(xs))) if range.end - start < xs.len() => {
                    let width = xs[range.end - start] - xs[range.start - start];
                    if hyphen {
                        width + hyphen_advance(range.end)
                    } else {
                        width
                    }
                }
                _ => measure(&range, hyphen),
            }
        },
    );
    let width = key.width.unwrap_or_else(|| {
        lines
            .iter()
            .map(|line| px(measure(&line.range, line.hyphen)))
            .max()
            .unwrap_or(px(0.0))
    });
    WrappedMail { lines, width }
}

fn dimmed_line(line: String, palette: &Palette) -> AnyElement {
    let mut label = div()
        .text_sm()
        .text_color(paint(palette.dimmed))
        .child(line);
    label.style().align_self = Some(gpui::AlignItems::FlexStart);
    label.into_any_element()
}

fn blocked_label(alts: &[&str], palette: &Palette) -> AnyElement {
    dimmed_line(blocked_text(alts), palette)
}

fn blocked_text(alts: &[&str]) -> String {
    let named: Vec<&str> = alts.iter().copied().filter(|alt| !alt.is_empty()).collect();
    if named.is_empty() {
        format!("{} images blocked", alts.len())
    } else {
        format!("Images blocked \u{b7} {}", named.join(", "))
    }
}

/// Remote images from `at` on, linked or not, with only spaces between:
/// where the run ends and each image's alt text. `None` when `at` is not
/// an image.
pub(crate) fn image_run(inlines: &[Inline], at: usize) -> Option<(usize, Vec<&str>)> {
    fn image_item<'a>(inline: &'a Inline, alts: &mut Vec<&'a str>) -> bool {
        match inline {
            Inline::Image {
                source: ImageSource::Remote(_),
                alt,
            } => {
                alts.push(alt.as_str());
                true
            }
            Inline::Link { children, .. } => {
                let mut inner = Vec::new();
                let images = children.iter().all(|child| match child {
                    Inline::Text(text) => text.chars().all(char::is_whitespace),
                    other => image_item(other, &mut inner),
                }) && !inner.is_empty();
                if images {
                    alts.extend(inner);
                }
                images
            }
            _ => false,
        }
    }
    let mut alts = Vec::new();
    if !image_item(inlines.get(at)?, &mut alts) {
        return None;
    }
    let mut end = at + 1;
    let mut next = end;
    while let Some(inline) = inlines.get(next) {
        match inline {
            Inline::Text(text) if text.chars().all(char::is_whitespace) => next += 1,
            Inline::LineBreak => next += 1,
            other if image_item(other, &mut alts) => {
                next += 1;
                end = next;
            }
            _ => break,
        }
    }
    Some((end, alts))
}

/// A paragraph that is only blocked remote images (a row of social icons,
/// each perhaps in a link) reads as one line: `Images blocked · Mastodon,
/// Photos, RSS`. `None` for anything else.
fn blocked_image_line(inlines: &[Inline], allow_remote: bool) -> Option<String> {
    fn collect<'a>(inlines: &'a [Inline], alts: &mut Vec<&'a str>) -> bool {
        inlines.iter().all(|inline| match inline {
            Inline::Image {
                source: ImageSource::Remote(_),
                alt,
            } => {
                alts.push(alt.as_str());
                true
            }
            Inline::Link { children, .. } => collect(children, alts),
            Inline::Text(text) => text.chars().all(|ch| ch.is_whitespace()),
            Inline::LineBreak => true,
            _ => false,
        })
    }
    if allow_remote {
        return None;
    }
    let mut alts = Vec::new();
    if !collect(inlines, &mut alts) || alts.len() < 2 {
        return None;
    }
    Some(blocked_text(&alts))
}

/// How far a wrapped `pre` line's continuation sits in from its start, so
/// a reader sees where the source line goes on.
const PRE_CONTINUATION_PX: f32 = 8.;

/// A `pre` block's rows at `columns` characters: each source line, broken
/// at the last space that fits (or at the column when there is none), with
/// continuation rows `indent` columns shorter. `true` marks a continuation.
pub(crate) fn wrap_pre(text: &str, columns: usize, indent: usize) -> Vec<(String, bool)> {
    let columns = columns.max(8);
    let indent = indent.min(columns / 2);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let line = line.replace('\t', "    ");
        let chars: Vec<char> = line.trim_end().chars().collect();
        if chars.is_empty() {
            rows.push((String::new(), false));
            continue;
        }
        let mut start = 0;
        let mut continuation = false;
        while start < chars.len() {
            let room = if continuation {
                columns - indent
            } else {
                columns
            };
            let mut end = (start + room).min(chars.len());
            if end < chars.len()
                && let Some(space) = chars[start..end].iter().rposition(|ch| *ch == ' ')
                && space > room / 3
            {
                end = start + space + 1;
            }
            let row: String = chars[start..end].iter().collect();
            rows.push((
                if continuation {
                    row.trim().to_string()
                } else {
                    row.trim_end().to_string()
                },
                continuation,
            ));
            start = end;
            continuation = true;
        }
    }
    rows
}

/// A code block whose long lines wrap with a continuation indent. The face
/// is monospace, so the width of a row is its characters times one
/// advance; the rows are laid out at the width the block is given and
/// painted one shaped line each.
struct PreText {
    text: SharedString,
    rows: Rc<RefCell<Vec<(String, bool)>>>,
    style: Option<(gpui::Font, Pixels, Pixels, gpui::Hsla)>,
    /// Pixels over the inherited size: a code the reader copies.
    grow: f32,
}

impl PreText {
    fn new(text: &str) -> Self {
        Self {
            text: SharedString::from(text.to_string()),
            rows: Rc::default(),
            style: None,
            grow: 0.,
        }
    }

    /// [`LARGE_CODE_GROW_PX`] over the body size, with room between lines.
    fn large(text: &str) -> Self {
        Self {
            grow: LARGE_CODE_GROW_PX,
            ..Self::new(text)
        }
    }
}

/// How much larger than the body a code set large in the mail (a sign-in
/// or booking code) is painted: big enough to read off and type, not the
/// sender's 24 to 32 px.
const LARGE_CODE_GROW_PX: f32 = 4.;

impl IntoElement for PreText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for PreText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = window.text_style();
        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size()) + px(self.grow);
        let line_height = if self.grow > 0. {
            font_size * 1.4
        } else {
            style
                .line_height
                .to_pixels(font_size.into(), window.rem_size())
        };
        let font_id = window.text_system().resolve_font(&font);
        let advance = window
            .text_system()
            .advance(font_id, font_size, 'm')
            .map(|size| size.width)
            .unwrap_or(font_size * 0.6);
        self.style = Some((font, font_size, line_height, style.color));
        let text = self.text.clone();
        let rows = Rc::clone(&self.rows);
        let indent = (PRE_CONTINUATION_PX / f32::from(advance).max(1.)).ceil() as usize;
        let layout = window.request_measured_layout(
            gpui::Style::default(),
            move |known, available, _, _| {
                let width = known.width.or(match available.width {
                    gpui::AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });
                let columns = match width {
                    Some(width) => (f32::from(width) / f32::from(advance).max(1.)).floor() as usize,
                    None => usize::MAX / 2,
                };
                let wrapped = wrap_pre(&text, columns, indent);
                let widest = wrapped
                    .iter()
                    .map(|(row, continuation)| {
                        row.chars().count() + if *continuation { indent } else { 0 }
                    })
                    .max()
                    .unwrap_or(0);
                let size = gpui::size(
                    (advance * widest as f32).min(width.unwrap_or(Pixels::MAX)),
                    line_height * wrapped.len() as f32,
                );
                *rows.borrow_mut() = wrapped;
                size
            },
        );
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some((font, font_size, line_height, color)) = self.style.clone() else {
            return;
        };
        for (at, (row, continuation)) in self.rows.borrow().iter().enumerate() {
            if row.is_empty() {
                continue;
            }
            let run = gpui::TextRun {
                len: row.len(),
                font: font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(row.clone()),
                font_size,
                &[run],
                None,
            );
            let x = if *continuation {
                px(PRE_CONTINUATION_PX)
            } else {
                px(0.)
            };
            let origin = gpui::point(
                bounds.origin.x + x,
                bounds.origin.y + line_height * at as f32,
            );
            let _ = shaped.paint(origin, line_height, window, cx);
        }
    }
}

fn highlight(mark: &Mark, palette: &Palette) -> Option<HighlightStyle> {
    if !mark.strong && !mark.emphasis && !mark.strike && !mark.code && mark.link.is_none() {
        return None;
    }
    Some(HighlightStyle {
        color: mark.link.as_ref().map(|_| ink(palette.accent)),
        font_weight: mark.strong.then_some(FontWeight::BOLD),
        font_style: mark.emphasis.then_some(FontStyle::Italic),
        background_color: mark.code.then(|| ink(palette.code_bg)),
        underline: mark.link.as_ref().map(|_| UnderlineStyle {
            thickness: px(1.),
            color: Some(ink(palette.accent)),
            wavy: false,
        }),
        strikethrough: mark.strike.then(|| gpui::StrikethroughStyle {
            thickness: px(1.),
            color: None,
        }),
        fade_out: None,
    })
}

fn image_el(
    source: &ImageSource,
    alt: &str,
    palette: &Palette,
    allow_remote: bool,
    gallery: &ImageGallery,
    layout: BlockColumn,
) -> AnyElement {
    if let ImageSource::Data { bytes, .. } = source
        && let Some((rendered, width, height)) = gallery.placed(bytes, palette.scheme() == "dark")
    {
        let (limit, inset) = layout.prose.map_or((width, 0.0), |prose| {
            if width > READING_MEASURE_PX {
                (layout.width, layout.wide_inset)
            } else {
                prose
            }
        });
        let shown_width = width.min(limit);
        let shown_height = height * (shown_width / width);
        let mut frame = div()
            .h(px(shown_height))
            .child(img(rendered).w_full().h_full());
        frame.style().align_self = Some(gpui::AlignItems::FlexStart);
        let frame = match layout.reader {
            Some(reader) => crate::view::disclosure::clip_column(
                inset,
                shown_width,
                if width > READING_MEASURE_PX {
                    reader.pane_right
                } else {
                    reader.card_right
                },
                frame,
            ),
            None => frame.w(px(shown_width)),
        };
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
                "Image".to_string()
            } else {
                format!("Image · {alt}")
            }
        }
        // A `cid:` is the attachment's id, not a name: the alt says what it is.
        ImageSource::Cid(_) | ImageSource::Data { .. } => {
            if alt.is_empty() {
                "Inline image".to_string()
            } else {
                format!("Inline image · {alt}")
            }
        }
    };
    let placeholder = div()
        .text_sm()
        .text_color(paint(crate::theme::note_text(
            palette.dimmed,
            layout
                .reader
                .map_or(palette.background, |reader| reader.fill),
        )))
        .child(label);
    let mut placeholder = layout.prose(placeholder);
    placeholder.style().align_self = Some(gpui::AlignItems::FlexStart);
    placeholder.into_any_element()
}

/// A mail image or a web snapshot. Far below the image crate's 512MiB default,
/// and checked before the pixels are allocated.
pub(crate) const MAX_IMAGE_EDGE: u32 = 8_192;
pub(crate) const MAX_IMAGE_BYTES: u64 = fell_render::MAX_DATA_IMAGE_BYTES as u64;

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

pub(super) fn image_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_IMAGE_BYTES);
    limits
}

#[cfg(test)]
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
    // One mask for both themes, so a diagonal is not lighter in one of them.
    let strokes = even_stroke_weight(&strokes);
    let strokes = crop_to_marks(&strokes);
    let (width, height) = display_size(strokes.width(), strokes.height());
    (
        render_frame(smooth_frame(
            &tint(&strokes, [0x3a, 0x3a, 0x3a]),
            width,
            height,
        )),
        render_frame(smooth_frame(
            &tint(&strokes, [0xc4, 0xc4, 0xc4]),
            width,
            height,
        )),
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
    let pad = 0;
    let x0 = min_x.saturating_sub(pad);
    let y0 = min_y.saturating_sub(pad);
    let x1 = (max_x + pad).min(width - 1);
    let y1 = (max_y + pad).min(height - 1);
    image::imageops::crop_imm(rgba, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image()
}

fn display_size(width: u32, height: u32) -> (f32, f32) {
    // Stay on the source pixels. Upscaling a line drawing turns the roof
    // into steps and a blur.
    (width.max(1) as f32, height.max(1) as f32)
}

/// Scale the sketch with a filter before it is shown. A GPU stretch of a
/// 1px line turns the roof into steps and a second edge.
fn smooth_frame(src: &image::RgbaImage, width: f32, height: f32) -> image::RgbaImage {
    let tw = width.round().max(1.) as u32;
    let th = height.round().max(1.) as u32;
    if tw == src.width() && th == src.height() {
        return src.clone();
    }
    image::imageops::resize(src, tw, th, image::imageops::FilterType::CatmullRom)
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
    let (&paper_key, &count) = counts.iter().max_by_key(|(_, n)| *n)?;
    if count * 100 / opaque.len() < 80 {
        return None;
    }
    let mut sum = [0u64; 3];
    let mut paper_pixels = 0u64;
    for px in &opaque {
        let key = (px[0] & !7, px[1] & !7, px[2] & !7);
        if key == paper_key {
            sum[0] += u64::from(px[0]);
            sum[1] += u64::from(px[1]);
            sum[2] += u64::from(px[2]);
            paper_pixels += 1;
        }
    }
    if paper_pixels == 0 {
        return None;
    }
    let paper = [
        (sum[0] / paper_pixels) as u8,
        (sum[1] / paper_pixels) as u8,
        (sum[2] / paper_pixels) as u8,
    ];
    let mut saw_ink = false;
    let mut out = rgba.clone();
    for px in out.pixels_mut() {
        let alpha = stroke_alpha(px.0, paper);
        if alpha == 0 {
            *px = image::Rgba([0, 0, 0, 0]);
            continue;
        }
        if !near_grayscale(px[0], px[1], px[2]) {
            return None;
        }
        saw_ink = true;
        // Coverage, not a hard mask. An anti-aliased roof pixel stays a soft
        // edge of the same stroke instead of a second solid line.
        *px = image::Rgba([255, 255, 255, alpha]);
    }
    saw_ink.then_some(out)
}

fn stroke_alpha(pixel: [u8; 4], paper: [u8; 3]) -> u8 {
    if pixel[3] < 16 {
        return 0;
    }
    let dist = pixel[0]
        .abs_diff(paper[0])
        .max(pixel[1].abs_diff(paper[1]))
        .max(pixel[2].abs_diff(paper[2]));
    // Keep the gray fringe of an antialiased stroke as partial coverage.
    let span = dist.saturating_sub(12);
    u8::try_from((u16::from(span) * 255) / 72).unwrap_or(255)
}

fn near_grayscale(red: u8, green: u8, blue: u8) -> bool {
    red.max(green).max(blue) - red.min(green).min(blue) <= 24
}

/// A pixel this dark is the stroke itself, not the pale anti-aliased fringe.
const STROKE_INK: u8 = 200;
/// Distance from the centerline that still counts as ink. A two-pixel wall
/// is centered on the crack, so its pixels sit 0.5 away and the next pixel
/// sits 1.5 away. 1.25 keeps the wall and picks up the diagonal's shoulder.
const STROKE_RADIUS: f32 = 1.25;

/// Redraw a paper-lifted sketch so every edge has the same weight.
///
/// Anti-aliased roof diagonals otherwise stay a pale fringe and read thinner
/// than the walls. A small rectangle of the same ink reads heavier. The
/// centerline is snapped onto the pixel grid and stroked at about two pixels,
/// the weight of the walls and the ground line. Each connected component is
/// judged on its own: a fill, or a mark whose centerline disappears, stays as
/// it was drawn. One thick shape must not decide the rest of the picture.
fn even_stroke_weight(src: &image::RgbaImage) -> image::RgbaImage {
    let (width, height) = src.dimensions();
    if width < 3 || height < 3 {
        return src.clone();
    }
    let mut mask = vec![0u8; (width * height) as usize];
    let mut ink = 0usize;
    for (i, px) in src.pixels().enumerate() {
        if px[3] >= STROKE_INK {
            mask[i] = 1;
            ink += 1;
        }
    }
    if ink == 0 {
        return src.clone();
    }
    let components = connected_ink(&mask, width, height);
    // The thinner never visits the outer pixel. A pad of paper lets a mark
    // on that frame be removed, then the pad is dropped.
    let thinned = thin_with_paper_pad(&mask, width, height);
    let mut centers: HashMap<(u32, u32), (f32, f32)> = HashMap::new();
    let mut kept: Vec<bool> = Vec::with_capacity(components.len());
    for pixels in &components {
        let survivors = pixels.iter().filter(|&&index| thinned[index] == 1).count();
        // Zero survivors is a mark the thinner erased. One survivor is the speck
        // a solid block leaves behind after the rest is peeled away. A stroke
        // that should be redrawn keeps a run of centerline pixels.
        let keep = component_is_thick(&mask, pixels, width, height)
            || (survivors < 2 && pixels.len() > survivors);
        if !keep {
            for &index in pixels {
                if thinned[index] == 0 {
                    continue;
                }
                let x = index as u32 % width;
                let y = index as u32 / width;
                centers.insert((x, y), stroke_centroid(&mask, width, height, x, y));
            }
        }
        kept.push(keep);
    }
    let mut coverage = vec![0u8; mask.len()];
    let points: Vec<(u32, u32)> = centers.keys().copied().collect();
    for &(x, y) in &points {
        let (cx, cy) = centers[&(x, y)];
        stamp_disk(&mut coverage, width, height, cx, cy);
        for (dx, dy) in [(1i32, 0i32), (-1, 1), (0, 1), (1, 1)] {
            let nx = i32::try_from(x).unwrap_or(0) + dx;
            let ny = i32::try_from(y).unwrap_or(0) + dy;
            if nx < 0 || ny < 0 {
                continue;
            }
            let (nx, ny) = (nx as u32, ny as u32);
            if nx >= width || ny >= height {
                continue;
            }
            let Some(&(ox, oy)) = centers.get(&(nx, ny)) else {
                continue;
            };
            stamp_segment(&mut coverage, width, height, cx, cy, ox, oy);
        }
    }
    let mut out = image::RgbaImage::new(width, height);
    for (px, alpha) in out.pixels_mut().zip(coverage) {
        if alpha > 0 {
            *px = image::Rgba([255, 255, 255, alpha]);
        }
    }
    // A kept mark wins on its own pixels, so a nearby stroke cannot paint over it.
    for (pixels, keep) in components.iter().zip(kept) {
        if !keep {
            continue;
        }
        for &index in pixels {
            let x = index as u32 % width;
            let y = index as u32 / width;
            *out.get_pixel_mut(x, y) = *src.get_pixel(x, y);
        }
    }
    out
}

/// Eight-connected groups of ink. Diagonal neighbors belong together, because
/// that is how the thinner looks at them.
fn connected_ink(mask: &[u8], width: u32, height: u32) -> Vec<Vec<usize>> {
    let mut labels = vec![0u32; mask.len()];
    let mut components = Vec::new();
    let w = width as i32;
    let h = height as i32;
    for start in 0..mask.len() {
        if mask[start] == 0 || labels[start] != 0 {
            continue;
        }
        let id = u32::try_from(components.len()).unwrap_or(u32::MAX) + 1;
        let mut pixels = Vec::new();
        let mut stack = vec![start];
        labels[start] = id;
        while let Some(current) = stack.pop() {
            pixels.push(current);
            let x = (current as u32 % width) as i32;
            let y = (current as u32 / width) as i32;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let xx = x + dx;
                    let yy = y + dy;
                    if xx < 0 || yy < 0 || xx >= w || yy >= h {
                        continue;
                    }
                    let next = (yy as u32 * width + xx as u32) as usize;
                    if mask[next] == 1 && labels[next] == 0 {
                        labels[next] = id;
                        stack.push(next);
                    }
                }
            }
        }
        components.push(pixels);
    }
    components
}

/// A filled shape has a pixel with no paper in a five-by-five window.
/// A four-by-four block does not, so this is not the test that keeps it.
fn component_is_thick(mask: &[u8], pixels: &[usize], width: u32, height: u32) -> bool {
    let w = width as i32;
    let h = height as i32;
    pixels.iter().any(|&index| {
        let x = (index as u32 % width) as i32;
        let y = (index as u32 / width) as i32;
        let mut paper = false;
        for dy in -2i32..=2 {
            for dx in -2i32..=2 {
                let xx = x + dx;
                let yy = y + dy;
                if xx < 0
                    || yy < 0
                    || xx >= w
                    || yy >= h
                    || mask[(yy as u32 * width + xx as u32) as usize] == 0
                {
                    paper = true;
                }
            }
        }
        !paper
    })
}

/// Thin as if the sketch sat one pixel in from the edge, then drop the pad.
fn thin_with_paper_pad(mask: &[u8], width: u32, height: u32) -> Vec<u8> {
    let padded_width = width + 2;
    let padded_height = height + 2;
    let mut padded = vec![0u8; (padded_width * padded_height) as usize];
    for y in 0..height {
        for x in 0..width {
            padded[((y + 1) * padded_width + (x + 1)) as usize] = mask[(y * width + x) as usize];
        }
    }
    thin_to_centerline(&mut padded, padded_width, padded_height);
    let mut thinned = vec![0u8; mask.len()];
    for y in 0..height {
        for x in 0..width {
            thinned[(y * width + x) as usize] = padded[((y + 1) * padded_width + (x + 1)) as usize];
        }
    }
    thinned
}

/// Zhang-Suen thinning. Endpoints stay, so a roof peak is not eaten.
fn thin_to_centerline(mask: &mut [u8], width: u32, height: u32) {
    let w = width as usize;
    let h = height as usize;
    if w < 3 || h < 3 {
        return;
    }
    let mut guard = width.saturating_mul(height);
    while guard > 0 {
        guard -= 1;
        let mut removed = false;
        for step in 0..2 {
            let mut kill = Vec::new();
            for y in 1..h - 1 {
                for x in 1..w - 1 {
                    if mask[y * w + x] == 0 {
                        continue;
                    }
                    let n = [
                        mask[(y - 1) * w + x],
                        mask[(y - 1) * w + x + 1],
                        mask[y * w + x + 1],
                        mask[(y + 1) * w + x + 1],
                        mask[(y + 1) * w + x],
                        mask[(y + 1) * w + x - 1],
                        mask[y * w + x - 1],
                        mask[(y - 1) * w + x - 1],
                    ];
                    let neighbors: u8 = n.iter().sum();
                    if !(2..=6).contains(&neighbors) {
                        continue;
                    }
                    let mut turns = 0;
                    for i in 0..8 {
                        if n[i] == 0 && n[(i + 1) % 8] == 1 {
                            turns += 1;
                        }
                    }
                    if turns != 1 {
                        continue;
                    }
                    let (p2, p4, p6, p8) = (n[0], n[2], n[4], n[6]);
                    let blocked = if step == 0 {
                        (p2 != 0 && p4 != 0 && p6 != 0) || (p4 != 0 && p6 != 0 && p8 != 0)
                    } else {
                        (p2 != 0 && p4 != 0 && p8 != 0) || (p2 != 0 && p6 != 0 && p8 != 0)
                    };
                    if blocked {
                        continue;
                    }
                    kill.push(y * w + x);
                }
            }
            if !kill.is_empty() {
                removed = true;
                for index in kill {
                    mask[index] = 0;
                }
            }
        }
        if !removed {
            break;
        }
    }
}

fn stroke_centroid(mask: &[u8], width: u32, height: u32, x: u32, y: u32) -> (f32, f32) {
    let mut sx = 0.0f32;
    let mut sy = 0.0f32;
    let mut n = 0.0f32;
    for dy in -1i32..=1 {
        for dx in -1i32..=1 {
            let xx = i32::try_from(x).unwrap_or(0) + dx;
            let yy = i32::try_from(y).unwrap_or(0) + dy;
            if xx < 0 || yy < 0 {
                continue;
            }
            let (xx, yy) = (xx as u32, yy as u32);
            if xx >= width || yy >= height || mask[(yy * width + xx) as usize] == 0 {
                continue;
            }
            sx += xx as f32 + 0.5;
            sy += yy as f32 + 0.5;
            n += 1.0;
        }
    }
    if n == 0.0 {
        (x as f32 + 0.5, y as f32 + 0.5)
    } else {
        (sx / n, sy / n)
    }
}

fn stroke_cover(dist: f32) -> u8 {
    if dist <= STROKE_RADIUS { 255 } else { 0 }
}

fn put_coverage(coverage: &mut [u8], width: u32, x: u32, y: u32, alpha: u8) {
    if alpha == 0 {
        return;
    }
    let index = (y * width + x) as usize;
    if alpha > coverage[index] {
        coverage[index] = alpha;
    }
}

fn stamp_disk(coverage: &mut [u8], width: u32, height: u32, cx: f32, cy: f32) {
    let x0 = (cx - 3.0).floor().max(0.0) as u32;
    let y0 = (cy - 3.0).floor().max(0.0) as u32;
    let x1 = (cx + 3.0).ceil().min((width - 1) as f32).max(0.0) as u32;
    let y1 = (cy + 3.0).ceil().min((height - 1) as f32).max(0.0) as u32;
    if x0 > x1 || y0 > y1 {
        return;
    }
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dist = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
            put_coverage(coverage, width, x, y, stroke_cover(dist));
        }
    }
}

fn stamp_segment(coverage: &mut [u8], width: u32, height: u32, ax: f32, ay: f32, bx: f32, by: f32) {
    let x0 = (ax.min(bx) - 3.0).floor().max(0.0) as u32;
    let y0 = (ay.min(by) - 3.0).floor().max(0.0) as u32;
    let x1 = (ax.max(bx) + 3.0).ceil().min((width - 1) as f32).max(0.0) as u32;
    let y1 = (ay.max(by) + 3.0).ceil().min((height - 1) as f32).max(0.0) as u32;
    if x0 > x1 || y0 > y1 {
        return;
    }
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dist = dist_to_segment(x as f32 + 0.5, y as f32 + 0.5, ax, ay, bx, by);
            put_coverage(coverage, width, x, y, stroke_cover(dist));
        }
    }
}

fn dist_to_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let abx = bx - ax;
    let aby = by - ay;
    let len2 = abx * abx + aby * aby;
    if len2 < 1.0e-6 {
        return (px - ax).hypot(py - ay);
    }
    let t = ((px - ax) * abx + (py - ay) * aby) / len2;
    let t = t.clamp(0.0, 1.0);
    (px - (ax + t * abx)).hypot(py - (ay + t * aby))
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
    use super::*;
    use fell_render::html_to_document;

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
                Inline::Emphasis(c)
                | Inline::Strong(c)
                | Inline::Strike(c)
                | Inline::Link { children: c, .. } => {
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
    fn cabin_strokes_share_one_weight() {
        use super::{even_stroke_weight, lift_paper};

        let doc = html_to_document(include_str!(
            "../../../fell-render/fixtures/native-personal-reply.html"
        ));
        let bytes = find_data(&doc.blocks).expect("fixture has a data image");
        let rgba = super::decode_rgba(bytes).expect("png");
        let lifted = lift_paper(&rgba).expect("sketch");
        let even = even_stroke_weight(&lifted);

        let wall = row_runs(&even, 60);
        assert_eq!(
            wall,
            vec![2, 2, 2, 2],
            "walls and the window are the same two-pixel stroke, got {wall:?}"
        );
        let ground = (0..even.height())
            .filter(|&y| row_runs(&even, y).iter().any(|run| *run > 40))
            .count();
        assert_eq!(ground, 2, "the ground line is two pixels tall");
        let roof = (0..40)
            .filter(|&y| {
                let runs = row_runs(&even, y);
                runs.len() == 2 && runs.iter().all(|run| *run == 2 || *run == 3)
            })
            .count();
        assert!(
            roof > 8,
            "roof diagonals stay a two-pixel stroke, snapped, not a pale fringe"
        );
        for px in even.pixels() {
            assert!(
                px[3] == 0 || px[3] == 255,
                "a partial alpha would make one edge look lighter"
            );
        }
        let mut fingerprint = 0u64;
        for (i, px) in even.pixels().enumerate() {
            if px[3] > 0 {
                fingerprint = fingerprint
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    .wrapping_add(((i as u64) << 8) | u64::from(px[3]));
            }
        }
        assert_eq!(
            fingerprint, 9_197_713_809_069_333_569,
            "the cabin the judges scored must keep the same pixels"
        );
    }

    #[test]
    fn small_solid_marks_survive_beside_a_stroke() {
        use super::even_stroke_weight;

        let mut image = image::RgbaImage::new(32, 32);
        for y in 4..28 {
            image.put_pixel(26, y, image::Rgba([255, 255, 255, 255]));
            image.put_pixel(27, y, image::Rgba([255, 255, 255, 255]));
        }
        for y in 4..6 {
            for x in 4..6 {
                image.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        for y in 12..16 {
            for x in 12..16 {
                image.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let even = even_stroke_weight(&image);
        for y in 4..6 {
            for x in 4..6 {
                assert_eq!(even.get_pixel(x, y).0[3], 255, "2x2 {x},{y}");
            }
        }
        for y in 12..16 {
            for x in 12..16 {
                assert_eq!(even.get_pixel(x, y).0[3], 255, "4x4 {x},{y}");
            }
        }
        assert_eq!(even.get_pixel(26, 16).0[3], 255, "the other stroke stays");
        assert_eq!(even.get_pixel(27, 16).0[3], 255, "the other stroke stays");
        // The blocks stay solid. They are not replaced by a thinner spine.
        assert_eq!(even.get_pixel(5, 5).0[3], 255);
        assert_eq!(even.get_pixel(13, 13).0[3], 255);
    }

    #[test]
    fn solid_block_on_the_frame_stays_solid() {
        use super::even_stroke_weight;

        let mut image = image::RgbaImage::new(32, 32);
        for y in 4..28 {
            image.put_pixel(26, y, image::Rgba([255, 255, 255, 255]));
            image.put_pixel(27, y, image::Rgba([255, 255, 255, 255]));
        }
        for y in 8..12 {
            for x in 0..4 {
                image.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let even = even_stroke_weight(&image);
        for y in 8..12 {
            for x in 0..4 {
                assert_eq!(even.get_pixel(x, y).0[3], 255, "frame 4x4 {x},{y}");
            }
        }
        assert_eq!(even.get_pixel(26, 16).0[3], 255, "the other stroke stays");
    }

    #[test]
    fn filled_mark_is_not_reduced_to_a_spine() {
        use super::even_stroke_weight;

        let mut image = image::RgbaImage::new(24, 24);
        for y in 4..20 {
            for x in 4..20 {
                image.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let even = even_stroke_weight(&image);
        assert_eq!(even.get_pixel(12, 12).0[3], 255);
        assert_eq!(even.get_pixel(6, 6).0[3], 255);
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
        assert!((80. ..220.).contains(&width), "{width}x{height}");
        assert!((40. ..120.).contains(&height), "{width}x{height}");
    }

    fn corpus_doc(name: &str) -> fell_render::StyledDocument {
        let mail = fell_render::corpus()
            .into_iter()
            .find(|mail| mail.name == name)
            .expect(name);
        fell_render::prepare(&mail.html, &mail.headers).document
    }

    /// Advances of a sans face, near enough to test the layout rule.
    fn advance(text: &str, bold: bool) -> f32 {
        text.chars().count() as f32 * if bold { 9.0 } else { 8.0 }
    }

    #[test]
    fn table_columns_get_their_longest_word_and_short_values_their_width() {
        let doc = corpus_doc("data-table");
        let Some(fell_render::Block::Table { header, rows, .. }) = doc
            .blocks
            .iter()
            .find(|block| matches!(block, fell_render::Block::Table { .. }))
        else {
            panic!("{:?}", doc.blocks);
        };
        let mut lines: Vec<Vec<Vec<String>>> = Vec::new();
        for row in std::iter::once(header).chain(rows.iter()) {
            lines.push(
                row.iter()
                    .map(|cell| {
                        let mut out = Vec::new();
                        push_cell_lines(cell, &mut out);
                        out
                    })
                    .collect(),
            );
        }
        let input: Vec<MeasuredRow<'_>> = lines
            .iter()
            .enumerate()
            .map(|(at, row)| {
                (
                    at == 0,
                    row.iter().map(|cell| (cell.as_slice(), at == 0)).collect(),
                )
            })
            .collect();
        let widths = column_widths(&input, advance);
        assert_eq!(widths.len(), 7);
        assert!(
            !super::table_columns_fit(&widths, 502.),
            "560 px conversation after padding"
        );
        assert!(super::table_columns_fit(&widths, 750.));
        assert!(widths[2].basis >= advance("142.0", false) + TABLE_CELL_PAD_PX);
        assert!(widths[4].basis >= advance("14.2", false) + TABLE_CELL_PAD_PX);
        // Station: at least "Agricultural", and its long name gives room to share.
        assert!(widths[0].min >= advance("Agricultural", false) + TABLE_CELL_PAD_PX);
        assert!(widths[0].max > widths[0].min);
        // Elevation: the header may wrap at its space, the numbers never.
        assert!(widths[1].basis >= advance("2,015", false) + TABLE_CELL_PAD_PX);
        assert!(widths[1].basis >= advance("Elevation", true) + TABLE_CELL_PAD_PX);
        assert!(widths[1].basis < advance("Elevation (m)", true) + TABLE_CELL_PAD_PX);
        // Notes: a sentence column starts at its longest word and takes the
        // most of what is left.
        assert_eq!(widths[6].basis, widths[6].min);
        let share = |w: &ColumnWidth| w.max - w.basis;
        assert!(widths[..6].iter().all(|w| share(w) < share(&widths[6])));
        // A bold total is measured bold.
        let receipt = corpus_doc("esp-receipt");
        let Some(fell_render::Block::Table { rows, .. }) = receipt
            .blocks
            .iter()
            .find(|block| matches!(block, fell_render::Block::Table { .. }))
        else {
            panic!();
        };
        assert!(blocks_are_bold(&rows[4][1]));
        assert!(!blocks_are_bold(&rows[0][1]));
    }

    #[test]
    fn the_same_pane_can_fit_a_small_table_and_stack_a_wider_one() {
        let columns = ["Period", "Subtotal", "Tax", "Paid"].map(|text| [text.to_string()]);
        let rows = ["February", "2,015.00", "9.512 %", "1,952.34"].map(|text| [text.to_string()]);
        let widths_for = |count| {
            column_widths(
                &[
                    (
                        true,
                        columns[..count]
                            .iter()
                            .map(|text| (text.as_slice(), true))
                            .collect(),
                    ),
                    (
                        false,
                        rows[..count]
                            .iter()
                            .map(|text| (text.as_slice(), false))
                            .collect(),
                    ),
                ],
                advance,
            )
        };
        assert!(super::table_columns_fit(&widths_for(2), 250.));
        assert!(!super::table_columns_fit(&widths_for(4), 250.));
    }

    #[test]
    fn a_quote_indents_prose_and_small_tables_without_moving_wide_children() {
        use super::BlockColumn;

        let layout =
            BlockColumn::reading((1040., 182.), (600., 402.), (1040., 1.), (1002., 1400.), 0);
        let (quote, rule) = layout.history(12., 2., false);
        assert_eq!(rule, Some(402.));
        assert_eq!(quote.prose, Some((588., 414.)));
        assert_eq!(
            (quote.width, quote.wide_inset),
            (layout.width, layout.wide_inset)
        );
        let placement = |column: BlockColumn, intrinsic| {
            let table = column.table(intrinsic, &[]);
            (table.width, table.inset)
        };
        assert_eq!(placement(quote, 170.), (170., 414.));
        assert_eq!(placement(quote, 600.), (588., 414.));
        assert_eq!(placement(quote, 900.), placement(layout, 900.));
        assert_eq!(placement(quote, 900.), (900., 252.));
        assert_eq!(placement(quote, 1400.), (1040., 182.));
        let (rtl_quote, rtl_rule) = layout.history(12., 2., true);
        assert_eq!(rtl_rule, Some(1000.));
        assert_eq!(rtl_quote.prose, Some((588., 402.)));
        assert_eq!(placement(rtl_quote, 900.), placement(quote, 900.));
    }

    #[test]
    fn root_lists_share_the_widest_marker_across_prose() {
        use super::{BlockColumn, LIST_MARKER_GAP_PX, widest_top_level_marker};

        let doc = fell_render::html_to_document(
            "<ol><li>First</li><li>Second <ol start='100'><li>Nested</li></ol></li></ol>\
             <p>Continue after review.</p><ol start='9'><li>Ninth</li><li>Tenth</li></ol>",
        );
        let shared = widest_top_level_marker(&doc.blocks, |label| label.len() as f32 * 9.);
        assert_eq!(shared, 27.);
        let column = BlockColumn::reading((740., 0.), (600., 70.), (740., 1.), (670., 740.), 0);
        for local in [18., 27.] {
            let indent = column.list_marker_width(local, shared) + LIST_MARKER_GAP_PX;
            assert_eq!(column.indented(indent, false).prose, Some((567., 103.)));
        }
        let nested = BlockColumn {
            list_depth: 1,
            ..column
        };
        assert_eq!(nested.list_marker_width(18., shared), 18.);
    }

    #[test]
    fn a_list_marker_indents_prose_without_narrowing_wide_blocks() {
        use super::{BlockColumn, ColumnWidth, LIST_MARKER_GAP_PX, LIST_MIN_MARKER_PX};
        let indent = LIST_MIN_MARKER_PX + LIST_MARKER_GAP_PX;

        let layout = BlockColumn::reading((740., 0.), (600., 70.), (740., 1.), (670., 740.), 0);
        let item = layout.indented(indent, false);
        assert_eq!(item.prose, Some((600. - indent, 70. + indent)));
        assert_eq!((item.width, item.wide_inset), (740., 0.));
        let columns = [ColumnWidth {
            min: 104.,
            max: 110.,
            basis: 104.,
        }; 7];
        let table = item.table(772., &columns);
        assert!(table.grid, "it fits with the full wide measure");
        assert_eq!(table, layout.table(772., &columns));
    }

    #[test]
    fn a_stacked_table_uses_the_prose_column_at_both_slide_positions() {
        use super::{BlockColumn, ColumnWidth, TableLayout};

        for inset in [0., 42.] {
            let layout = BlockColumn::reading(
                (702., 0.),
                (600., inset),
                (702., 1.),
                (inset + 600., 702.),
                0,
            );
            let columns = [ColumnWidth {
                min: 108.,
                max: 188.,
                basis: 108.,
            }; 7];
            assert_eq!(
                layout.table(1002., &columns),
                TableLayout {
                    width: 600.,
                    inset,
                    right: inset + 600.,
                    grid: false
                },
            );
            let columns = [ColumnWidth {
                min: 95.,
                max: 150.,
                basis: 95.,
            }; 7];
            assert_eq!(
                layout.table(1002., &columns),
                TableLayout {
                    width: 702.,
                    inset: 0.,
                    right: 702.,
                    grid: true
                },
            );
        }
    }

    #[test]
    fn blocked_images_side_by_side_are_one_run() {
        let doc = corpus_doc("esp-marketing");
        let social = doc
            .blocks
            .iter()
            .find_map(|block| match block {
                fell_render::Block::Paragraph { inlines }
                    if blocked_image_line(inlines, false).is_some() =>
                {
                    blocked_image_line(inlines, false)
                }
                _ => None,
            })
            .expect("the social row");
        assert_eq!(social, "Images blocked \u{b7} Social feed, Photos, Videos");
        let inlines = vec![
            Inline::Text("Follow us ".into()),
            Inline::Link {
                href: "https://a.test".into(),
                children: vec![Inline::Image {
                    source: ImageSource::Remote("https://a.test/1.png".into()),
                    alt: "One".into(),
                }],
            },
            Inline::Text("\u{a0} ".into()),
            Inline::Image {
                source: ImageSource::Remote("https://a.test/2.png".into()),
                alt: "Two".into(),
            },
            Inline::Text(" today".into()),
        ];
        assert_eq!(image_run(&inlines, 0), None);
        let (end, alts) = image_run(&inlines, 1).expect("a run");
        assert_eq!((end, alts), (4, vec!["One", "Two"]));
    }

    #[test]
    fn wrapped_pre_lines_continue_indented() {
        let rows = wrap_pre(
            "short\nERROR worker[3]: connection failed after 3 retries",
            24,
            1,
        );
        assert_eq!(rows[0], ("short".to_string(), false));
        assert_eq!(rows[1], ("ERROR worker[3]:".to_string(), false));
        assert!(rows[2..].iter().all(|(_, continuation)| *continuation));
        assert!(
            rows.iter()
                .all(|(row, cont)| row.chars().count() <= if *cont { 23 } else { 24 })
        );
        let joined: String = rows[1..]
            .iter()
            .map(|(row, _)| row.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, "ERROR worker[3]: connection failed after 3 retries");
        // A long word with no space breaks at the column.
        let rows = wrap_pre(&"x".repeat(30), 10, 1);
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn right_to_left_paragraphs_wrap_at_spaces() {
        assert!(is_right_to_left("\u{5e9}\u{5dc}\u{5d5}\u{5dd} Avi Peretz"));
        assert!(!is_right_to_left("Avi \u{5e9}\u{5dc}\u{5d5}\u{5dd}"));
        assert!(!is_right_to_left("18:00"));
        let text = "aaa bbb ccc\nddd";
        let lines = wrap_words(text, Some(7.0), |range| range.len() as f32);
        let lines: Vec<&str> = lines.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(lines, ["aaa bbb", "ccc", "ddd"]);
    }

    #[test]
    fn typographic_apostrophes_stay_within_words_at_a_line_break() {
        for text in ["we don’t go", "we don't go", "we can’t go", "we won’t go"] {
            let lines = wrap_words(text, Some(6.0), |range| text[range].chars().count() as f32);
            let lines: Vec<_> = lines.iter().map(|range| &text[range.clone()]).collect();
            assert_eq!(lines, ["we", &text[3..text.len() - 3], "go"]);
        }
    }

    #[test]
    fn unicode_wrap_respects_nonbreaking_spaces_and_mandatory_separators() {
        let text = "go A\u{a0}B\u{2028}猫犬鳥";
        let lines = wrap_words(text, Some(4.0), |range| text[range].chars().count() as f32);
        let lines: Vec<_> = lines.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(lines, ["go", "A\u{a0}B", "猫犬鳥"]);
        let text = "猫犬鳥";
        let lines = wrap_words(text, Some(2.0), |range| text[range].chars().count() as f32);
        let lines: Vec<_> = lines.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(lines, ["猫犬", "鳥"]);
    }

    #[test]
    fn wrapping_preserves_blank_lines_between_paragraphs_and_at_the_end() {
        let text = "First paragraph.\n\nSecond paragraph.\n";
        let lines = wrap_words(text, Some(20.0), |range| range.len() as f32);
        let lines: Vec<_> = lines.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(lines, ["First paragraph.", "", "Second paragraph.", ""]);
    }

    #[test]
    fn a_soft_hyphen_only_prints_at_a_discretionary_break() {
        use super::{visible_mail_line, wrap_mail};

        let show = |text: &str, width| {
            wrap_mail(text, width, &[], &[], |range, hyphen| {
                visible_mail_line(text, &range, hyphen).chars().count() as f32
            })
            .into_iter()
            .map(|line| visible_mail_line(text, &line.range, line.hyphen).into_owned())
            .collect::<Vec<_>>()
        };
        assert_eq!(show("well\u{ad}known", Some(9.0)), ["wellknown"]);
        assert_eq!(show("well\u{ad}known", Some(5.0)), ["well-", "known"]);
        assert_eq!(show("well\u{ad}known", Some(4.0)), ["well", "know", "n"]);
        assert_eq!(show("well\u{ad}\nknown", Some(9.0)), ["well", "known"]);
        assert_eq!(show("well\u{ad}known", None), ["wellknown"]);
    }

    #[test]
    fn soft_hyphen_breaks_reuse_one_hard_line_measurement_at_each_width() {
        use std::cell::Cell;

        use super::{WrapKey, byte_xs, visible_mail_line, wrap_mail, wrap_measured_mail};

        let text = "well\u{ad}known café ".repeat(1000);
        for width in [5.0, 16.0] {
            let key = WrapKey {
                text: text.clone().into(),
                fonts: Vec::new(),
                keep: Vec::new(),
                urls: Vec::new(),
                size: gpui::px(14.0),
                width: Some(gpui::px(width)),
            };
            let shapes = Cell::new(0);
            let measure =
                |range: &_, hyphen| visible_mail_line(&text, range, hyphen).chars().count() as f32;
            let fast = wrap_measured_mail(
                &key,
                false,
                |range| {
                    shapes.set(shapes.get() + 1);
                    let stripped = visible_mail_line(&text, range, false);
                    let glyphs = stripped
                        .char_indices()
                        .enumerate()
                        .map(|(x, (byte, _))| (byte, x as f32));
                    byte_xs(
                        glyphs,
                        &text[range.clone()],
                        stripped.chars().count() as f32,
                    )
                },
                |range, hyphen| {
                    shapes.set(shapes.get() + 1);
                    measure(range, hyphen)
                },
                |_| 1.0,
            );
            let reference = wrap_mail(&text, Some(width), &[], &[], |range, hyphen| {
                measure(&range, hyphen)
            });
            assert_eq!(shapes.get(), 1);
            assert!(fast.lines.len() >= 1000);
            assert!(
                fast.lines
                    .iter()
                    .map(|line| (&line.range, line.hyphen))
                    .eq(reference.iter().map(|line| (&line.range, line.hyphen)))
            );
        }
    }

    #[test]
    fn a_url_prefers_separators_and_keeps_its_original_bytes() {
        let text = "https://example.test/north/south/section-four?mode=view&format=text";
        let url = 0..text.len();
        let lines: Vec<_> = super::wrap_mail(
            text,
            Some(29.0),
            &[],
            std::slice::from_ref(&url),
            |range, _| range.len() as f32,
        )
        .into_iter()
        .map(|line| &text[line.range])
        .collect();
        assert_eq!(
            lines,
            [
                "https://example.test/north/",
                "south/section-four?mode=view&",
                "format=text"
            ]
        );
        assert_eq!(lines.concat(), text);
    }

    #[test]
    fn a_long_mail_is_wrapped_once_at_a_stable_width() {
        use super::{WrapCache, WrapKey, WrappedMail, wrap_mail};
        use std::rc::Rc;

        let mut cache = WrapCache::default();
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(1500);
        let key = WrapKey {
            fonts: vec![(text.len(), gpui::font("sans-serif"))],
            text: text.into(),
            keep: Vec::new(),
            urls: Vec::new(),
            size: gpui::px(14.),
            width: Some(gpui::px(600.)),
        };
        let first = cache.get_or_wrap(key.clone(), |key| {
            let width = key.width.unwrap();
            let lines = wrap_mail(
                &key.text,
                Some(width.into()),
                &key.keep,
                &key.urls,
                |range, _| range.len() as f32 * 7.0,
            );
            WrappedMail { lines, width }
        });
        assert!(first.lines.len() > 500);
        for _ in 0..12 {
            let frame = cache.get_or_wrap(key.clone(), |_| panic!("rewrapped at the same width"));
            assert!(Rc::ptr_eq(&frame, &first));
        }
        let different_width = WrapKey {
            width: Some(gpui::px(850.)),
            ..key
        };
        let frame = cache.get_or_wrap(different_width, |_| WrappedMail::default());
        assert!(!Rc::ptr_eq(&frame, &first));
    }

    #[test]
    fn english_led_mixed_direction_glyphs_need_range_shaping() {
        let text = "Hi אבג now";
        assert!(!super::is_right_to_left(text));
        // The Hebrew word's clusters go backward in visual glyph order.
        let visual_order = [0, 1, 2, 7, 5, 3, 9, 10, 11, 12];
        let glyphs = visual_order
            .into_iter()
            .enumerate()
            .map(|(x, byte)| (byte, x as f32));
        assert!(super::byte_xs(glyphs, text, 10.0).is_none());
        assert_eq!(
            super::byte_xs([(0, 0.0), (1, 2.0), (1, 3.0)].into_iter(), "abc", 5.0),
            Some(vec![0.0, 2.0, 5.0, 5.0])
        );
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn inline_code_stays_whole_unless_it_is_wider_than_the_line() {
        let width = |range: Range<usize>, _| range.len() as f32;
        let show = |text: &str, lines: Vec<super::MailLine>| -> Vec<String> {
            lines
                .iter()
                .map(|line| text[line.range.clone()].to_string())
                .collect()
        };
        // Do not take a Unicode break inside inline code.
        let text = "change it to /mnt/archive when";
        let lines = super::wrap_mail(text, Some(16.0), &[13..25], &[], width);
        assert_eq!(show(text, lines), ["change it to", "/mnt/archive", "when"]);
        // Code with spaces stays on one line when it fits.
        let text = "try retries = 5 on staging";
        let lines = super::wrap_mail(text, Some(14.0), &[4..15], &[], width);
        assert_eq!(show(text, lines), ["try", "retries = 5 on", "staging"]);
        // Wider than the line: it breaks at its spaces first.
        let lines = super::wrap_mail(text, Some(9.0), &[4..15], &[], width);
        assert_eq!(show(text, lines), ["try", "retries =", "5 on", "staging"]);
        // Normal text uses Unicode's break opportunity after a slash.
        let text = "see https://a.test/very/long/path ok";
        let lines = super::wrap_mail(text, Some(12.0), &[], &[], width);
        assert_eq!(
            show(text, lines),
            ["see https://", "a.test/very/", "long/path ok"]
        );
    }

    #[test]
    fn an_overwide_word_breaks_only_between_graphemes() {
        let text = "e\u{301}e\u{301}e\u{301}";
        let lines = wrap_words(text, Some(3.0), |range| text[range].chars().count() as f32);
        let lines: Vec<_> = lines.iter().map(|range| &text[range.clone()]).collect();
        assert_eq!(lines, ["e\u{301}"; 3]);
    }

    #[test]
    fn blocked_caption_shares_the_sketch_and_the_quote_keeps_the_paragraph_gap() {
        use super::{BLOCK_GAP, CAPTION_GAP, caption_under_sketch, column_runs};
        use fell_render::ImageSource;

        assert_eq!(CAPTION_GAP * 2., BLOCK_GAP);

        let doc = html_to_document(include_str!(
            "../../../fell-render/fixtures/native-personal-reply.html"
        ));
        let gallery = super::ImageGallery::from_document(&doc);
        let sketch = doc
            .blocks
            .iter()
            .position(|block| super::decoded_sketch(block, &gallery))
            .expect("sketch");
        let runs = column_runs(&doc.blocks, false, &gallery);
        assert!(
            runs.contains(&(sketch, sketch + 2)),
            "caption stays with the sketch: {runs:?}"
        );
        assert!(
            runs.contains(&(sketch + 2, sketch + 3)),
            "the quote stays its own block: {runs:?}"
        );
        assert!(matches!(doc.blocks[sketch + 2], Block::Quote { .. }));
        assert!(caption_under_sketch(
            &doc.blocks[sketch],
            doc.blocks.get(sketch + 1),
            false,
            &gallery
        ));
        let allowed = column_runs(&doc.blocks, true, &gallery);
        assert!(
            allowed.contains(&(sketch, sketch + 1)),
            "an allowed picture keeps the paragraph gap: {allowed:?}"
        );
        assert!(!caption_under_sketch(
            &doc.blocks[sketch],
            doc.blocks.get(sketch + 1),
            true,
            &gallery
        ));
        assert!(!caption_under_sketch(
            &doc.blocks[sketch - 1],
            Some(&doc.blocks[sketch]),
            false,
            &gallery
        ));
        match super::sole_image(&doc.blocks[sketch + 1]) {
            Some(ImageSource::Remote(url)) => assert!(url.contains("cabin"), "{url}"),
            other => panic!("caption is the blocked cabin image, got {other:?}"),
        }

        let sketch_block = doc.blocks[sketch].clone();
        let doubled = fell_render::StyledDocument {
            blocks: vec![sketch_block.clone(), sketch_block.clone()],
        };
        let both = super::ImageGallery::from_document(&doubled);
        assert!(
            !caption_under_sketch(&doubled.blocks[0], Some(&doubled.blocks[1]), true, &both),
            "two sketches keep the paragraph gap"
        );

        let missing = fell_render::StyledDocument {
            blocks: vec![
                sketch_block,
                Block::Image {
                    source: ImageSource::Cid("photo".into()),
                    alt: "photo".into(),
                },
            ],
        };
        let missing_gallery = super::ImageGallery::from_document(&missing);
        assert!(
            caption_under_sketch(
                &missing.blocks[0],
                Some(&missing.blocks[1]),
                true,
                &missing_gallery
            ),
            "a missing image stays the gray caption"
        );
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

    fn row_runs(image: &image::RgbaImage, y: u32) -> Vec<u32> {
        let mut runs = Vec::new();
        let mut run = 0u32;
        for x in 0..image.width() {
            if image.get_pixel(x, y).0[3] == 255 {
                run += 1;
            } else if run > 0 {
                runs.push(run);
                run = 0;
            }
        }
        if run > 0 {
            runs.push(run);
        }
        runs
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
                | fell_render::Inline::Strike(children)
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
