//! Where to cut a shaped line so it fits a width.
//!
//! A row's subject and snippet are shaped once, whole, and cut from that
//! shaping. The cut falls only where the shaper starts a cluster, so a
//! grapheme it drew as one (an emoji ZWJ sequence, a flag, a letter with
//! its marks) is kept or dropped whole. What is kept is a logical prefix
//! of the text, whatever the direction it is drawn in: the width of a
//! prefix is the sum of the advances of the glyphs it owns, wherever the
//! bidi reordering put them on the line.

use std::collections::BTreeMap;

/// The places a shaped line can be cut, in text order, and how wide the
/// text before each one is.
#[derive(Debug)]
pub struct LineCuts {
    /// Byte index of each cluster start, ascending; the last is the text's
    /// length.
    starts: Vec<usize>,
    /// `widths[i]`: the width of the text before `starts[i]`.
    widths: Vec<f32>,
}

impl LineCuts {
    /// `glyphs`: each glyph's cluster start (a byte index into the text)
    /// and its x on the line, in any order. `width`: the line's width.
    /// `len`: the text's length in bytes.
    pub fn new(glyphs: impl IntoIterator<Item = (usize, f32)>, width: f32, len: usize) -> Self {
        let mut glyphs: Vec<(usize, f32)> = glyphs.into_iter().collect();
        // Left to right as drawn: a glyph's advance runs to the next one.
        glyphs.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut advances: BTreeMap<usize, f32> = BTreeMap::new();
        advances.insert(0, 0.0);
        for (index, &(start, x)) in glyphs.iter().enumerate() {
            let next = glyphs.get(index + 1).map_or(width, |glyph| glyph.1);
            *advances.entry(start.min(len)).or_default() += (next - x).max(0.0);
        }
        let mut starts = Vec::with_capacity(advances.len() + 1);
        let mut widths = Vec::with_capacity(advances.len() + 1);
        let mut before = 0.0;
        for (start, advance) in advances {
            if start == len {
                break;
            }
            starts.push(start);
            widths.push(before);
            before += advance;
        }
        starts.push(len);
        widths.push(before);
        Self { starts, widths }
    }

    /// The end of the longest prefix at most `room` wide that ends on a
    /// cluster start before byte `before` (a binary search). 0 when none
    /// fits.
    pub fn fit(&self, room: f32, before: usize) -> usize {
        let candidates = self.starts.partition_point(|start| *start < before);
        let fits = self.widths[..candidates].partition_point(|width| *width <= room);
        fits.checked_sub(1).map_or(0, |last| self.starts[last])
    }

    /// The cluster starts, for tests.
    #[cfg(test)]
    fn starts(&self) -> &[usize] {
        &self.starts
    }
}

/// What a cut keeps of `prefix` before the ellipsis: no trailing space,
/// and no joiner left waiting for what was cut off.
pub fn kept(prefix: &str) -> &str {
    prefix.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{200d}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic_text::{Attrs, AttrsList, FontSystem, ShapeBuffer, ShapeLine, Shaping, Wrap};
    use std::sync::{Mutex, OnceLock};

    const PX: f32 = 14.0;

    /// One font system for every test: loading the system fonts is slow.
    fn fonts() -> &'static Mutex<FontSystem> {
        static FONTS: OnceLock<Mutex<FontSystem>> = OnceLock::new();
        FONTS.get_or_init(|| Mutex::new(FontSystem::new()))
    }

    /// `text` shaped as GPUI shapes a line on Linux: each glyph's cluster
    /// start and x, and the line's width.
    fn shape(text: &str) -> (Vec<(usize, f32)>, f32) {
        let mut fonts = fonts().lock().unwrap();
        let attrs = AttrsList::new(&Attrs::new());
        let line = ShapeLine::new(&mut fonts, text, &attrs, Shaping::Advanced, 4);
        let mut lines = Vec::new();
        line.layout_to_buffer(
            &mut ShapeBuffer::default(),
            PX,
            None,
            Wrap::None,
            None,
            &mut lines,
            None,
        );
        let Some(layout) = lines.first() else {
            return (Vec::new(), 0.0);
        };
        let glyphs = layout
            .glyphs
            .iter()
            .map(|glyph| (glyph.start, glyph.x))
            .collect();
        (glyphs, layout.w)
    }

    fn cuts(text: &str) -> LineCuts {
        let (glyphs, width) = shape(text);
        LineCuts::new(glyphs, width, text.len())
    }

    /// Every room from nothing to the whole line, a pixel apart.
    fn rooms(text: &str) -> impl Iterator<Item = f32> {
        let width = shape(text).1;
        (0..=width.ceil() as usize + 1).map(|px| px as f32)
    }

    /// The cut for every room is a logical prefix whose own shaping fits
    /// (a pixel of tolerance for shaping a prefix on its own), and the
    /// next cluster would not have.
    fn assert_cuts_are_longest_fitting_prefixes(text: &str) {
        let cuts = cuts(text);
        for room in rooms(text) {
            let end = cuts.fit(room, text.len() + 1);
            assert!(text.is_char_boundary(end));
            let width = shape(&text[..end]).1;
            assert!(
                width <= room + 1.0,
                "{text:?} room {room}: {end} is {width} wide"
            );
            if let Some(&next) = cuts.starts().iter().find(|start| **start > end) {
                let wider = shape(&text[..next]).1;
                assert!(
                    wider > room - 1.0,
                    "{text:?} room {room}: {next} fits too ({wider})"
                );
            }
        }
    }

    #[test]
    fn a_plain_line_cuts_at_each_letter() {
        // Glyphs a, b, c at 0, 10, 18; the line is 30 wide.
        let cuts = LineCuts::new([(0, 0.0), (1, 10.0), (2, 18.0)], 30.0, 3);
        assert_eq!(cuts.fit(9.0, 4), 0);
        assert_eq!(cuts.fit(10.0, 4), 1);
        assert_eq!(cuts.fit(25.0, 4), 2);
        assert_eq!(cuts.fit(30.0, 4), 3);
        assert_eq!(cuts.fit(30.0, 3), 2, "never at or past `before`");
    }

    #[test]
    fn glyphs_of_one_cluster_are_kept_together() {
        // "e" + a combining acute (3 bytes), then "x": the mark has its
        // own glyph, drawn over the e, in the same cluster.
        let cuts = LineCuts::new([(0, 0.0), (0, 2.0), (3, 8.0)], 16.0, 4);
        assert_eq!(cuts.starts(), &[0, 3, 4]);
        assert_eq!(cuts.fit(5.0, 5), 0, "half the cluster is not a cut");
        assert_eq!(cuts.fit(8.0, 5), 3);
    }

    #[test]
    fn a_right_to_left_line_cuts_by_logical_index() {
        // Three letters of 1 byte each drawn right to left: the first
        // letter is the rightmost glyph.
        let cuts = LineCuts::new([(2, 0.0), (1, 6.0), (0, 14.0)], 24.0, 3);
        // The first letter is 10 wide, the second 8, the third 6.
        assert_eq!(cuts.fit(9.0, 4), 0);
        assert_eq!(cuts.fit(10.0, 4), 1);
        assert_eq!(cuts.fit(18.0, 4), 2);
        assert_eq!(cuts.fit(24.0, 4), 3);
    }

    #[test]
    fn kept_drops_trailing_space_and_joiners() {
        assert_eq!(kept("hello  "), "hello");
        assert_eq!(kept("a\u{1f468}\u{200d}"), "a\u{1f468}");
        assert_eq!(kept(""), "");
    }

    #[test]
    fn an_emoji_zwj_sequence_is_never_split() {
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
        let text = format!("Hi {family} and {family}!");
        let cuts = cuts(&text);
        let families: Vec<usize> = text.match_indices(family).map(|(at, _)| at).collect();
        for room in rooms(&text) {
            let end = cuts.fit(room, text.len() + 1);
            let split = families
                .iter()
                .any(|at| end > *at && end < at + family.len());
            assert!(!split, "room {room}: cut at {end} splits a family");
        }
    }

    #[test]
    fn flags_are_never_split() {
        // US, FR, DE, JP: two regional indicators of 4 bytes each.
        let text = "\u{1f1fa}\u{1f1f8}\u{1f1eb}\u{1f1f7}\u{1f1e9}\u{1f1ea}\u{1f1ef}\u{1f1f5}";
        let cuts = cuts(text);
        for room in rooms(text) {
            let end = cuts.fit(room, text.len() + 1);
            assert_eq!(end % 8, 0, "room {room}: cut at {end} splits a flag");
        }
    }

    #[test]
    fn a_right_to_left_subject_cuts_to_its_longest_fitting_prefix() {
        assert_cuts_are_longest_fitting_prefixes("שלום עולם, מה שלומך היום");
    }

    #[test]
    fn mixed_direction_text_cuts_to_its_longest_fitting_prefix() {
        assert_cuts_are_longest_fitting_prefixes("Re: שלום עולם and the 2026 plan");
        assert_cuts_are_longest_fitting_prefixes("שלום Fell 42 עולם!");
    }

    #[test]
    fn a_long_sender_leaves_room_for_the_ellipsis() {
        let sender = "Northwind Trail Station, Equipment and Route Planning Notifications";
        let budget = 180.0;
        let cuts = cuts(sender);
        let end = cuts.fit(budget - shape("…").1, sender.len());
        assert!(end > 0 && end < sender.len());
        let visible = crate::present::ellipsis(kept(&sender[..end]));
        assert!(shape(&visible).1 <= budget + 1.0, "{visible:?}");
    }
}
