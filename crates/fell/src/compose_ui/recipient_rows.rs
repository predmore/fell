use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::Instant;

use fell_core::compose::{AddressView, parse_address_line};
use gpui::{
    Bounds, FontStyle, Pixels, ScrollHandle, StatefulInteractiveElement, TextAlign, TextRun,
    UnderlineStyle, Window, canvas, div, fill, point, prelude::*, px, rgb, size,
};
use unicode_segmentation::UnicodeSegmentation;

use super::{LABEL_PX, Palette, display_safe};
use crate::motion::{self, Glide, Motion};

pub(super) const FIELDS_PADDING: f32 = 16.;
pub(super) const FIELD_GAP: f32 = 12.;
pub(super) const FIELD_INDENT: f32 = 4.;
pub(super) const FIELD_RULE: f32 = 2.;

pub(super) fn label_width(window: &Window) -> Pixels {
    px(LABEL_PX).min(window.viewport_size().width / 4.)
}

const LINE: Pixels = px(22.);
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
}

struct Span {
    bytes: Range<usize>,
    kind: SpanKind,
}

enum SpanKind {
    Invalid,
    Selected,
    Note(u32),
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
    };
    let saved: HashMap<_, _> = saved
        .iter()
        .map(|address| (address.raw.as_str(), address))
        .collect();
    for (at, raw) in tokens(value) {
        let trimmed = raw.trim();
        if at > 0 {
            if wide || trimmed.is_empty() {
                shown.text.push(' ');
            } else {
                shown.text.push('\n');
            }
        }
        let skip = raw.len() - raw.trim_start().len();
        let visible = raw.trim_start();
        let start = shown.text.len();
        shown.text.push_str(&display_safe(visible));
        let end = shown.text.len();
        let invalid = show_errors && parse_address_line(raw).is_err();
        let mut selected = false;
        if let Some(range) = &selection
            && (at..=at + raw.len()).contains(&range.start)
        {
            let display_at = |source: usize| {
                start + display_safe(&visible[..source.saturating_sub(at + skip)]).len()
            };
            let left = display_at(range.start);
            let right = display_at(range.end);
            shown.caret = Some(left);
            if left < right {
                selected = true;
                shown.spans.push(Span {
                    bytes: left..right,
                    kind: SpanKind::Selected,
                });
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
                    kind: SpanKind::Note(color),
                });
            }
        }
        if at + raw.len() < value.len() && (wide || value[at + raw.len() + 1..].trim().is_empty()) {
            shown.text.push(',');
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

/// Keep a whole line visible even while the row is growing or when a long
/// list needs to scroll in a small window.
fn reveal_offset(full: Pixels, height: Pixels, current: Pixels, anchor: Option<Pixels>) -> Pixels {
    let top = anchor.map_or(current, |y| current.clamp(y + LINE - height, y));
    top.clamp(px(0.), (full - height).max(px(0.)))
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
) -> gpui::Div {
    let mut shown = display_line(line, colors, wide);
    let mut style = window.text_style();
    style.font_family = family.to_owned().into();
    style.font_weight = gpui::FontWeight(400.);
    style.color = rgb(colors.fg).into();
    let mut runs = Vec::<TextRun>::new();
    let mut notes = Vec::new();
    let mut at = 0;
    for span in shown.spans {
        if at < span.bytes.start {
            runs.push(style.to_run(span.bytes.start - at));
        }
        let mut run = style.clone();
        match span.kind {
            SpanKind::Invalid => {
                run.color = rgb(colors.error).into();
                run.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: None,
                    wavy: true,
                });
            }
            SpanKind::Selected => {
                run.background_color = Some(rgb(colors.selection).into());
                let fg = crate::theme::ensure_all(
                    colors.fg,
                    &[colors.selection],
                    crate::theme::TEXT_CONTRAST,
                );
                run.color = rgb(fg).into();
            }
            SpanKind::Note(color) => {
                notes.push(span.bytes.clone());
                run.color = rgb(color).into();
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
    let viewport = window.viewport_size();
    // Match the fields' padding and the row's accent rule, label and gap.
    let chrome = 2. * FIELDS_PADDING + FIELD_RULE + FIELD_INDENT + FIELD_GAP + 4.;
    let width = (viewport.width - px(chrome) - label_width(window) - CARET).max(px(1.));
    let shape = |text: String| {
        window
            .text_system()
            .shape_text(text.into(), px(16.), &runs, Some(width), None)
            .unwrap_or_default()
    };
    let mut lines = shape(shown.text.clone());
    // GPUI can break at punctuation inside a note. If that happened, wrap
    // before the note instead, keeping its address and the note legible.
    let mut start = 0;
    let mut moved_note = false;
    for line in &lines {
        for boundary in line.wrap_boundaries() {
            let glyph = &line.runs()[boundary.run_ix].glyphs[boundary.glyph_ix];
            let at = start + glyph.index;
            let note = notes.partition_point(|note| note.end <= at);
            if let Some(note) = notes.get(note)
                && note.start < at
                && shown.text.as_bytes()[note.start] == b' '
            {
                shown.text.replace_range(note.start..note.start + 1, "\n");
                moved_note = true;
            }
        }
        start += line.len() + 1;
    }
    if moved_note {
        lines = shape(shown.text);
    }
    let mut full_height = px(0.);
    let mut start = 0;
    let mut caret = None;
    let mut invalid = None;
    for line in &lines {
        let end = start + line.len();
        let position = |at| {
            line.position_for_index(at - start, LINE)
                .map(|p| point(p.x.min(width), p.y + full_height))
        };
        if let Some(at) = shown.caret.filter(|at| (start..=end).contains(at)) {
            caret = position(at);
        }
        if let Some(at) = shown.invalid.filter(|at| (start..=end).contains(at)) {
            invalid = position(at);
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
    if row.glide.is_some() {
        window.request_animation_frame();
    }

    let offset = reveal_offset(
        full_height,
        height,
        -row.scroll.offset().y,
        caret.or(invalid).map(|p| p.y),
    );
    row.scroll.set_offset(point(px(0.), -offset));

    let painted = canvas(
        |_, _, _| (),
        move |bounds, (), window, cx| {
            let mut origin = bounds.origin;
            for line in &lines {
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
    div().child(
        div()
            .id(label)
            .h(height)
            .overflow_y_scroll()
            .track_scroll(&row.scroll)
            .child(painted),
    )
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
                matches!(span.kind, SpanKind::Selected) && &focused.text[span.bytes.clone()] == "r"
            }));
            assert_eq!(focused.caret, inactive.invalid);
        }
    }

    #[test]
    fn the_caret_line_stays_visible_during_growth_and_after_resize() {
        let motion = Motion::new(true);
        let start = Instant::now();
        let mut row = RowHeight::default();
        assert_eq!(row.height(22., motion, start), 22.);
        assert_eq!(row.height(44., motion, start), 22.);
        let full = LINE * 2;
        for elapsed in [0, 16, 60, 120] {
            let now = start + std::time::Duration::from_millis(elapsed);
            let height = px(row.height(44., motion, now));
            let skip = reveal_offset(full, height, px(0.), Some(LINE));
            assert!(height >= LINE && height <= full);
            assert!(LINE >= skip && full <= skip + height);
            if elapsed == 60 {
                assert!(
                    height > px(33.) && height < full,
                    "ease-out is past the midpoint"
                );
            }
        }
        assert_eq!(
            reveal_offset(LINE * 10, LINE * 3, px(0.), Some(LINE * 8)),
            LINE * 6
        );
        assert_eq!(
            reveal_offset(LINE * 10, LINE * 3, LINE * 6, Some(LINE)),
            LINE
        );
        assert_eq!(
            row.height(22., Motion::new(false), start + motion::SLIDE),
            22.
        );
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
