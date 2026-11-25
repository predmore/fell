use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

use fell_core::compose::{AddressView, parse_address_line, recipient_display_char};
use gpui::{
    App, Bounds, FontStyle, MouseButton, Pixels, Point, ScrollHandle, StatefulInteractiveElement,
    TextAlign, TextRun, UnderlineStyle, Window, WrappedLineLayout, canvas, div, fill, point,
    prelude::*, px, rgb, size,
};
use unicode_segmentation::UnicodeSegmentation;

use super::{LABEL_PX, Palette};
use crate::motion::{self, Glide, Motion, Transition};

pub(super) const FIELDS_PADDING: f32 = 16.;
pub(super) const FIELD_GAP: f32 = 12.;
pub(super) const FIELD_INDENT: f32 = 4.;
pub(super) const FIELD_RULE: f32 = 2.;

pub(super) fn label_width(window: &Window) -> Pixels {
    px(LABEL_PX).min(window.viewport_size().width / 4.)
}

pub(super) const LINE: Pixels = px(26.);
const CARET: Pixels = px(3.);

pub(super) fn matches_saved(line: &str, addresses: &[AddressView]) -> bool {
    parse_address_line(line).is_ok_and(|line| {
        line.iter()
            .map(String::as_str)
            .eq(addresses.iter().map(|address| address.raw.as_str()))
    })
}

pub(super) fn tokens(line: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut quoted = false;
    let mut escaped = false;
    line.split(move |ch| {
        if std::mem::take(&mut escaped) {
            return false;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => return true,
            _ => {}
        }
        false
    })
    .scan(0, |start, raw| {
        let at = *start;
        *start += raw.len() + 1;
        Some((at, raw))
    })
}

pub(super) fn first_invalid(line: &str) -> Option<Range<usize>> {
    tokens(line).find_map(|(at, raw)| {
        let left = raw.len() - raw.trim_start().len();
        parse_address_line(raw).err()?;
        Some(at + left..at + raw.trim_end().len())
    })
}

/// A new field starts at the end unless it is reopening a rejected token.
pub(super) fn edit(
    line: &mut String,
    selection: &mut Option<Range<usize>>,
    key: &str,
    typed: Option<&str>,
) {
    let mut range = selection.clone().unwrap_or(line.len()..line.len());
    let before = || {
        line[..range.start]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(at, _)| at)
    };
    let after = || range.end + line[range.end..].graphemes(true).next().map_or(0, str::len);
    let at = match key {
        "left" if !range.is_empty() => range.start,
        "right" if !range.is_empty() => range.end,
        "left" => before(),
        "right" => after(),
        "home" => 0,
        "end" => line.len(),
        "backspace" | "delete" => {
            if range.is_empty() {
                if key == "backspace" {
                    range.start = before();
                } else {
                    range.end = after();
                }
            }
            let at = range.start;
            line.replace_range(range, "");
            at
        }
        _ => {
            let Some(text) = typed else { return };
            let at = range.start + text.len();
            line.replace_range(range, text);
            at
        }
    };
    *selection = (at != line.len()).then_some(at..at);
}

/// Focus and selection only change decoration. The two states use the same
/// text, including the notes and the room at the right for the insertion bar.
struct DisplayLine {
    text: String,
    spans: Vec<Span>,
    caret: Option<usize>,
    invalid: Option<usize>,
    selected: Option<usize>,
    source_offsets: Vec<(usize, usize)>,
    keep_together: Vec<Range<usize>>,
}

struct Span {
    bytes: Range<usize>,
    kind: SpanKind,
}

enum SpanKind {
    Invalid,
    SelectedInvalid,
    Note { color: u32, recipient: String },
    Placeholder(u32),
}

pub(super) struct RecipientLine<'a> {
    pub value: &'a str,
    pub saved: &'a [AddressView],
    pub redirects: &'a HashSet<String>,
    pub placeholder: (&'a str, u32),
    pub selection: Option<Range<usize>>,
    pub show_errors: bool,
}

fn display_line(line: RecipientLine<'_>, colors: Palette, wide: bool) -> DisplayLine {
    let RecipientLine {
        value,
        saved,
        redirects,
        placeholder,
        selection,
        show_errors,
    } = line;
    let mut shown = DisplayLine {
        text: String::new(),
        spans: Vec::new(),
        caret: None,
        invalid: None,
        selected: None,
        source_offsets: Vec::new(),
        keep_together: Vec::new(),
    };
    let saved: HashMap<_, _> = saved
        .iter()
        .map(|address| (address.raw.as_str(), address))
        .collect();
    for (at, raw) in tokens(value) {
        let trimmed = raw.trim();
        let separator = shown.text.len();
        if at > 0 {
            shown.text.push(if wide { ' ' } else { '\n' });
        }
        let skip = raw.len() - raw.trim_start().len();
        let source_start = at + skip;
        let visible = raw.trim_start();
        let start = shown.text.len();
        for (offset, ch) in visible.char_indices() {
            shown
                .source_offsets
                .push((shown.text.len(), source_start + offset));
            shown.text.push(if recipient_display_char(ch) {
                ch
            } else {
                '�'
            });
        }
        let end = shown.text.len();
        shown.source_offsets.push((end, at + raw.len()));

        let invalid = show_errors && parse_address_line(raw).is_err();
        let mut selected = false;
        if let Some(range) = &selection
            && (at..=at + raw.len()).contains(&range.start)
        {
            let offset_for_source = |source: usize| {
                shown
                    .source_offsets
                    .iter()
                    .rev()
                    .find(|(_, byte)| *byte <= source.max(source_start))
                    .map_or(start, |(display, _)| *display)
            };
            let left = offset_for_source(range.start);
            let right = offset_for_source(range.end);
            if left < right {
                selected = true;
                shown.selected = Some(left);
                shown.spans.push(Span {
                    bytes: left..right,
                    kind: SpanKind::SelectedInvalid,
                });
            } else {
                shown.caret = Some(left);
            }
        }
        if invalid {
            shown.invalid.get_or_insert(start);
            if !selected && start < end {
                shown.spans.push(Span {
                    bytes: start..end,
                    kind: SpanKind::Invalid,
                });
            }
        }
        if wide && at > 0 && start < end {
            shown.keep_together.push(separator..end);
        }
        if let Some(address) = saved.get(trimmed) {
            let note = match (
                !redirects.is_empty() && redirects.contains(&address.email.trim().to_lowercase()),
                address.fresh,
            ) {
                (true, true) => Some(("(Reply-To · first time)", colors.error)),
                (true, false) => Some(("(Reply-To)", colors.dim)),
                (false, true) => Some(("(first time)", colors.dim)),
                _ => None,
            };
            if let Some((note, color)) = note {
                let start = shown.text.len();
                shown.text.push(' ');
                shown.text.push_str(&note.replace(' ', "\u{a0}"));
                shown.spans.push(Span {
                    bytes: start..shown.text.len(),
                    kind: SpanKind::Note {
                        color,
                        recipient: address.raw.clone(),
                    },
                });
                shown.keep_together.push(start..shown.text.len());
            }
        }
        if wide && at + raw.len() < value.len() {
            shown.text.push(',');
            if let Some(group) = shown.keep_together.last_mut()
                && group.end + 1 == shown.text.len()
            {
                group.end += 1;
            }
        }
    }
    if shown.text.is_empty() {
        let (words, color) = placeholder;
        if selection.is_some() {
            shown.caret = Some(0);
            shown.text.push_str("  ");
        }
        if selection.is_none() || words != "none" {
            let start = shown.text.len();
            shown.text.push_str(words);
            shown.spans.push(Span {
                bytes: start..shown.text.len(),
                kind: SpanKind::Placeholder(color),
            });
        }
    }
    shown
}

#[derive(Default)]
pub(super) struct RowHeight {
    target: Option<f32>,
    glide: Option<Glide>,
    scroll: ScrollHandle,
    invalid_anchor: Option<Pixels>,
    notes: HashMap<String, Option<Transition>>,
}

impl RowHeight {
    fn height(&mut self, target: f32, motion: Motion, now: Instant) -> f32 {
        if let Some(old) = self.target.replace(target)
            && old != target
        {
            let drawn = self.glide.map_or(old, |glide| glide.value(now));
            self.glide = Glide::start(&motion, drawn, target, 0., motion::SLIDE, now);
        }
        self.glide = self.glide.filter(|glide| glide.running(now));
        self.glide.map_or(target, |glide| glide.value(now))
    }
}

/// Leave the text pinned while the row opens below it. At its height cap,
/// scroll a full caret line into view. A caret above view reveals immediately.
fn reveal_offset(
    full: Pixels,
    height: Pixels,
    current: Pixels,
    anchor: Option<Pixels>,
    growing: bool,
) -> Pixels {
    let top = match anchor {
        Some(y) if y < current => y,
        Some(y) if !growing => current.max(y + LINE - height),
        _ => current,
    };
    top.clamp(px(0.), (full - height).max(px(0.)))
}

fn cursor_position(line: &WrappedLineLayout, at: usize, width: Pixels) -> Option<Point<Pixels>> {
    let mut position = line.position_for_index(at, LINE)?;
    if line
        .wrap_boundaries()
        .iter()
        .any(|boundary| line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index == at)
    {
        position.x = px(0.);
        position.y += LINE;
    }
    position.x = position.x.clamp(px(0.), width);
    Some(position)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn value_view(
    label: &'static str,
    line: RecipientLine<'_>,
    colors: Palette,
    wide: bool,
    family: &str,
    row: &mut RowHeight,
    motion: Motion,
    now: Instant,
    window: &mut Window,
    on_cursor: impl Fn(usize, &mut App) + 'static,
) -> gpui::Div {
    let mut shown = display_line(line, colors, wide);
    let mut style = window.text_style();
    style.font_family = family.to_owned().into();
    style.font_weight = gpui::FontWeight(400.);
    style.color = rgb(colors.fg).into();
    let mut runs = Vec::<TextRun>::new();
    let mut notes = HashMap::new();
    let mut notes_animating = false;
    let mut at = 0;
    for span in shown.spans {
        if at < span.bytes.start {
            runs.push(style.to_run(span.bytes.start - at));
        }
        let mut run = style.clone();
        match span.kind {
            SpanKind::Invalid | SpanKind::SelectedInvalid => {
                let selected = matches!(span.kind, SpanKind::SelectedInvalid);
                let error = if selected {
                    let bg = motion::mix(colors.panel, colors.error, 0.20);
                    run.background_color = Some(rgb(bg).into());
                    crate::theme::ensure_all(colors.error, &[bg], crate::theme::TEXT_CONTRAST)
                } else {
                    colors.error
                };
                run.color = rgb(error).into();
                run.underline = Some(UnderlineStyle {
                    thickness: px(1.5),
                    color: None,
                    wavy: false,
                });
            }
            SpanKind::Note { color, recipient } => {
                let fade = *notes.entry(recipient.clone()).or_insert_with(|| {
                    row.notes
                        .get(&recipient)
                        .copied()
                        .unwrap_or_else(|| row.target.and_then(|_| motion.start(motion::FADE, now)))
                });
                notes_animating |= fade.is_some_and(|fade| fade.running(now));
                run.color = rgb(motion::mix(
                    colors.panel,
                    color,
                    motion::progress(fade.as_ref(), now),
                ))
                .into();
                run.font_style = FontStyle::Italic;
            }
            SpanKind::Placeholder(color) => run.color = rgb(color).into(),
        }
        runs.push(run.to_run(span.bytes.len()));
        at = span.bytes.end;
    }
    if at < shown.text.len() {
        runs.push(style.to_run(shown.text.len() - at));
    }
    row.notes = notes;
    let viewport = window.viewport_size();
    // Match the fields' padding and the row's accent rule, label and gap.
    let chrome = 2. * FIELDS_PADDING + FIELD_RULE + FIELD_INDENT + FIELD_GAP + 4.;
    let width = (viewport.width - px(chrome) - label_width(window) - CARET).max(px(1.));
    let shape = |text: String, wrap| {
        window
            .text_system()
            .shape_text(text.into(), px(16.), &runs, wrap, None)
            .unwrap_or_default()
    };
    // Place hard breaks at measured group boundaries. GPUI still wraps a
    // single address that cannot fit on an otherwise empty line.
    if !shown.keep_together.is_empty() {
        let mut start = 0;
        let mut groups = shown.keep_together.iter().peekable();
        for line in shape(shown.text.clone(), None) {
            let mut left = px(0.);
            let end = start + line.len();
            while let Some(group) = groups.next_if(|group| group.start < end) {
                if group.start >= start
                    && line.unwrapped_layout.x_for_index(group.end - start) - left > width
                {
                    shown.text.replace_range(group.start..group.start + 1, "\n");
                    left = line.unwrapped_layout.x_for_index(group.start + 1 - start);
                }
            }
            start = end + 1;
        }
    }
    let lines = Rc::new(shape(shown.text, Some(width)));
    let mut full_height = px(0.);
    let mut start = 0;
    let mut caret = None;
    let mut invalid = None;
    let mut selected = None;
    for line in lines.iter() {
        let end = start + line.len();
        let position =
            |at| cursor_position(line, at - start, width).map(|p| point(p.x, p.y + full_height));
        if let Some(at) = shown.caret.filter(|at| (start..=end).contains(at)) {
            caret = position(at);
        }
        if let Some(at) = shown.invalid.filter(|at| (start..=end).contains(at)) {
            invalid = position(at);
        }
        if let Some(at) = shown.selected.filter(|at| (start..=end).contains(at)) {
            selected = position(at);
        }
        full_height += line.size(LINE).height;
        start = end + 1;
    }
    let full_height = full_height.max(LINE);
    // Preserve room for the other rows and the message when a list spans
    // more lines than fit comfortably in the header.
    let max_height = LINE
        * (f32::from(viewport.height / 5.) / f32::from(LINE))
            .floor()
            .max(1.);
    let height = px(row.height(f32::from(full_height.min(max_height)), motion, now));
    if notes_animating || row.glide.is_some() {
        window.request_animation_frame();
    }

    let invalid = invalid.map(|p| p.y);
    let new_invalid = row.invalid_anchor != invalid;
    row.invalid_anchor = invalid;
    let growing = row.glide.is_some_and(|glide| glide.from < glide.to);
    let offset = reveal_offset(
        full_height,
        height,
        -row.scroll.offset().y,
        caret.or(selected).map(|p| p.y).or_else(|| {
            if new_invalid || row.glide.is_some() {
                invalid
            } else {
                None
            }
        }),
        growing,
    );
    row.scroll.set_offset(point(px(0.), -offset));

    let origin = Rc::new(Cell::new(Point::default()));
    let prepaint_origin = Rc::clone(&origin);
    let paint_lines = Rc::clone(&lines);
    let painted = canvas(
        move |bounds, _, _| prepaint_origin.set(bounds.origin),
        move |bounds, (), window, cx| {
            let mut origin = bounds.origin;
            for line in paint_lines.iter() {
                let _ = line.paint_background(origin, LINE, TextAlign::Left, None, window, cx);
                let _ = line.paint(origin, LINE, TextAlign::Left, None, window, cx);
                origin.y += line.size(LINE).height;
            }
            if let Some(caret) = caret {
                window.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + caret + point(px(0.), px(2.)),
                        size(CARET, LINE - px(4.)),
                    ),
                    rgb(colors.caret),
                ));
            }
        },
    )
    .w_full()
    .h(full_height);
    let offsets = shown.source_offsets;
    let scroll = row.scroll.clone();
    div()
        .relative()
        .child(
            div()
                .id(label)
                .h(height)
                .overflow_y_scroll()
                .track_scroll(&row.scroll)
                .cursor_text()
                .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                    let mut position = event.position - origin.get();
                    let mut start = 0;
                    for line in lines.iter() {
                        let line_height = line.size(LINE).height;
                        if position.y < line_height {
                            let index = line
                                .closest_index_for_position(position, LINE)
                                .unwrap_or_else(|bound| bound);
                            let at = start + index;
                            let source = offsets.partition_point(|(display, _)| *display <= at);
                            on_cursor(
                                offsets
                                    .get(source.saturating_sub(1))
                                    .map_or(0, |(_, byte)| *byte),
                                cx,
                            );
                            return;
                        }
                        position.y -= line_height;
                        start += line.len() + 1;
                    }
                })
                .child(painted),
        )
        .children((full_height > max_height && full_height > height).then(|| {
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    let ratio = height / full_height;
                    let thumb = (height * ratio).max(px(8.));
                    let travel = height - thumb;
                    let scroll_fraction =
                        (-scroll.offset().y / (full_height - height)).clamp(0., 1.);
                    window.paint_quad(fill(
                        Bounds::new(
                            bounds.origin + point(px(0.), travel * scroll_fraction),
                            size(px(2.), thumb),
                        ),
                        rgb(colors.marker),
                    ));
                },
            )
            .absolute()
            .right_0()
            .top_0()
            .w(px(2.))
            .h(height)
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose_ui::{ThemeKind, palette};

    #[test]
    fn focusing_leaves_wrap_points_and_notes_in_place() {
        let raw = r#""Vale, Ari" <ari@example.test>, Avery Park <avery@example.test>, r"#;
        let saved = [AddressView {
            raw: r#""Vale, Ari" <ari@example.test>"#.into(),
            email: "ari@example.test".into(),
            fresh: true,
        }];
        let colors = palette(ThemeKind::Dark);
        let redirects = HashSet::new();
        let placeholder = ("none", colors.dim);
        let error = first_invalid(raw).unwrap();
        assert_eq!(&raw[error.clone()], "r");

        for wide in [false, true] {
            let shown = |selection| {
                display_line(
                    RecipientLine {
                        value: raw,
                        saved: &saved,
                        redirects: &redirects,
                        placeholder,
                        selection,
                        show_errors: true,
                    },
                    colors,
                    wide,
                )
            };
            let inactive = shown(None);
            let focused = shown(Some(error.clone()));
            assert_eq!(inactive.text, focused.text);
            assert_eq!(inactive.text.lines().count(), if wide { 1 } else { 3 });
            assert!(inactive.text.contains("(first\u{a0}time)"));
            assert!(inactive.spans.iter().any(|span| {
                matches!(span.kind, SpanKind::Invalid) && &inactive.text[span.bytes.clone()] == "r"
            }));
            assert!(focused.spans.iter().any(|span| {
                matches!(span.kind, SpanKind::SelectedInvalid)
                    && &focused.text[span.bytes.clone()] == "r"
            }));
            assert_eq!(focused.selected, inactive.invalid);
            assert!(focused.caret.is_none());
        }
    }

    #[test]
    fn growth_keeps_previous_text_pinned_and_tall_lists_scroll() {
        let motion = Motion::new(true);
        let start = Instant::now();
        let mut row = RowHeight::default();
        assert_eq!(row.height(26., motion, start), 26.);
        assert_eq!(row.height(52., motion, start), 26.);
        let full = LINE * 2;
        for elapsed in [0, 16, 60, 120] {
            let now = start + std::time::Duration::from_millis(elapsed);
            let height = px(row.height(52., motion, now));
            let skip = reveal_offset(full, height, px(0.), Some(LINE), elapsed < 120);
            assert!(height >= LINE && height <= full);
            assert_eq!(skip, px(0.));
            if elapsed == 60 {
                assert!(
                    height > px(39.) && height < full,
                    "ease-out is past the midpoint"
                );
            }
        }
        assert_eq!(
            reveal_offset(LINE * 10, LINE * 3, px(0.), Some(LINE * 8), false),
            LINE * 6
        );
        assert_eq!(
            reveal_offset(LINE * 10, LINE * 3, LINE * 6, Some(LINE), true),
            LINE
        );
        assert_eq!(
            row.height(26., Motion::new(false), start + motion::SLIDE),
            26.
        );
    }

    #[test]
    fn a_soft_wrap_boundary_places_the_caret_and_hit_target_on_the_continuation() {
        struct Empty;
        impl Render for Empty {
            fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
                div()
            }
        }
        let mut app = gpui::TestAppContext::single();
        app.add_window_view(|window, _| {
            let text = "ann@example.test";
            let width = px(65.);
            let run = window.text_style().to_run(text.len());
            let lines = window
                .text_system()
                .shape_text(text.into(), px(16.), &[run], Some(width), None)
                .unwrap();
            let line = &lines[0];
            let boundary = &line.wrap_boundaries()[0];
            let at = line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix].index;
            assert!(at > 0 && at < text.len());
            assert_eq!(line.position_for_index(at, LINE).unwrap().y, px(0.));
            let caret = cursor_position(line, at, width).unwrap();
            assert_eq!(caret, point(px(0.), LINE));
            assert_eq!(line.closest_index_for_position(caret, LINE), Ok(at));
            Empty
        });
    }

    #[test]
    fn a_narrow_comma_moves_the_caret_and_hidden_formatting_remains_selectable() {
        let colors = palette(ThemeKind::Dark);
        let redirects = HashSet::new();
        let shown = |value, selection| {
            display_line(
                RecipientLine {
                    value,
                    saved: &[],
                    redirects: &redirects,
                    placeholder: ("none", colors.dim),
                    selection,
                    show_errors: true,
                },
                colors,
                false,
            )
        };
        let complete = "ann@example.test,";
        let next = shown(complete, Some(complete.len()..complete.len()));
        assert_eq!(next.text, "ann@example.test\n");
        assert_eq!(next.caret, Some(next.text.len()));

        let hidden = "ann@ex\u{202e}ample.test";
        let error = first_invalid(hidden).unwrap();
        assert_eq!(&hidden[error.clone()], hidden);
        let invalid = shown(hidden, Some(error));
        assert_eq!(invalid.text, "ann@ex�ample.test");
        assert!(matches!(invalid.spans[0].kind, SpanKind::SelectedInvalid));
        assert_eq!(invalid.spans[0].bytes, 0..invalid.text.len());
    }

    #[test]
    fn replacing_or_deleting_an_invalid_token_keeps_the_suffix() {
        let mut line = r#""Vale, Ari" <ari@example.test>, 🔎bad, avery@example.test"#.to_owned();
        let mut selection = first_invalid(&line);
        assert_eq!(&line[selection.clone().unwrap()], "🔎bad");
        edit(&mut line, &mut selection, "a", Some("ada@example.test"));
        assert!(first_invalid(&line).is_none());
        edit(&mut line, &mut selection, "left", None);
        edit(&mut line, &mut selection, "delete", None);
        assert_eq!(
            line,
            r#""Vale, Ari" <ari@example.test>, ada@example.tes, avery@example.test"#
        );
        let unclosed = r#"ari@example.test, "Avery, Park"#;
        assert_eq!(
            &unclosed[first_invalid(unclosed).unwrap()],
            r#""Avery, Park"#
        );
    }
}
