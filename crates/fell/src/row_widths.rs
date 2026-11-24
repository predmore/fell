//! How a thread row lays out its subject, and how it moves when the row
//! turns read, unread, starred or unstarred.
//!
//! The subject box eases from the width it had on screen to the width of
//! the text at its new weight. At rest read and unread rows leave the same
//! gap after the subject. The unread bar starts fading when the subject
//! switches weight. The snippet follows the subject as the box grows or
//! shrinks.
//! Stars fade and scale in and out independently.
//! With reduced motion each change shows immediately.

use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::motion::{self, Motion, Transition};

/// Space between the subject and the snippet.
pub const SUBJECT_GAP: f32 = 8.0;

/// The longest a subject box gets before it is cut short.
pub const SUBJECT_MAX: f32 = 280.0;

/// A star appearing or leaving: a small glyph, so a touch quicker than a fade.
pub const STAR: Duration = Duration::from_millis(100);

/// The scale a star grows from as it fades in, and shrinks to as it leaves.
pub const STAR_FROM: f32 = 0.8;

/// How wide the fade is that softens the subject's clipped right edge.
pub const SUBJECT_FADE: f32 = 12.0;

/// A row as one frame draws it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowFrame {
    /// The subject box's width now (it eases while the box grows or relaxes).
    pub subject_w: f32,
    /// The width the subject text is cut to: where the box is going.
    pub subject_room: f32,
    /// How much of the unread bar shows: 0 read, 1 unread.
    pub unread: f32,
    /// The weight of the sender and subject text in this frame.
    pub bold: bool,
    /// How much of the star shows (its opacity; its scale follows).
    pub star: f32,
}

impl RowFrame {
    /// A row drawn as it is, with nothing moving: `subject_w` wide.
    pub fn still(subject_w: f32, unread: bool, starred: bool) -> Self {
        Self {
            subject_w,
            subject_room: subject_w,
            unread: f32::from(u8::from(unread)),
            bold: unread,
            star: f32::from(u8::from(starred)),
        }
    }

    /// The star's scale this frame.
    pub fn star_scale(&self) -> f32 {
        motion::lerp(STAR_FROM, 1.0, self.star)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct UnreadChange {
    old_box: f32,
    old_accent: f32,
    fade: Transition,
}

#[derive(Clone, Debug, PartialEq)]
struct Kept {
    /// The subject the widths belong to.
    subject: String,
    /// The box width at rest: where it is, or where it is easing to.
    width: f32,
    unread: bool,
    starred: bool,
    unread_change: Option<UnreadChange>,
    /// The star fading in (`starred`) or out.
    star: Option<Transition>,
    attached: Option<bool>,
    attachment: Option<Transition>,
    labels: Option<Vec<String>>,
    previous_labels: Option<(Vec<String>, Transition)>,
}

impl Kept {
    fn frame(&self, now: Instant) -> RowFrame {
        let mut frame = RowFrame::still(self.width, self.unread, self.starred);
        if let Some(change) = self.unread_change {
            let progress = change.fade.progress(now);
            frame.subject_w = motion::lerp(change.old_box, self.width, progress);
            frame.unread = motion::lerp(change.old_accent, frame.unread, progress);
        }
        let star = motion::progress(self.star.as_ref(), now);
        frame.star = if self.starred { star } else { 1.0 - star };
        frame
    }

    fn running(&self, now: Instant) -> bool {
        self.unread_change
            .is_some_and(|change| change.fade.running(now))
            || self.star.is_some_and(|t| t.running(now))
            || self.attachment.is_some_and(|t| t.running(now))
            || self
                .previous_labels
                .as_ref()
                .is_some_and(|(_, t)| t.running(now))
    }
}

/// The rows on screen as last drawn, by thread id.
#[derive(Debug)]
pub struct RowWidths {
    motion: Motion,
    rows: HashMap<String, Kept>,
}

impl Default for RowWidths {
    fn default() -> Self {
        Self::new(Motion::new(true))
    }
}

impl RowWidths {
    pub fn new(motion: Motion) -> Self {
        Self {
            motion,
            rows: HashMap::new(),
        }
    }

    /// A new folder, list, or font: every row lays out anew.
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    /// The list drew `range`; a row it left out scrolled off and is
    /// forgotten. `ids` holds every row in the list.
    pub fn keep_drawn<'a>(&mut self, range: Range<usize>, ids: impl Iterator<Item = &'a str>) {
        let drawn: std::collections::HashSet<&str> =
            ids.skip(range.start).take(range.len()).collect();
        self.rows.retain(|id, _| drawn.contains(id.as_str()));
    }

    /// Row `id` as the frame at `now` draws it. `natural(unread)` is the
    /// subject's width at that weight; it is asked only for the weight the
    /// row has, and only when the row first shows or its weight changes.
    /// `record` is false on a pass that only measures, which must not count
    /// as the row being on screen and starts nothing.
    pub fn frame(
        &mut self,
        id: &str,
        subject: &str,
        (unread, starred): (bool, bool),
        natural: impl Fn(bool) -> f32,
        now: Instant,
        record: bool,
    ) -> RowFrame {
        let motion = self.motion;
        let Some(kept) = self.rows.get_mut(id).filter(|kept| kept.subject == subject) else {
            // First on screen, or its subject changed (new mail): lay out anew.
            let width = natural(unread);
            if record {
                self.rows.insert(
                    id.to_string(),
                    Kept {
                        subject: subject.to_string(),
                        width,
                        unread,
                        starred,
                        unread_change: None,
                        star: None,
                        attached: None,
                        attachment: None,
                        labels: None,
                        previous_labels: None,
                    },
                );
            }
            return RowFrame::still(width, unread, starred);
        };
        if !record {
            return kept.frame(now);
        }
        if kept
            .unread_change
            .is_some_and(|change| !change.fade.running(now))
        {
            kept.unread_change = None;
        }
        if kept.star.is_some_and(|t| !t.running(now)) {
            kept.star = None;
        }
        if unread != kept.unread {
            let drawn = kept.frame(now);
            kept.unread = unread;
            kept.width = natural(unread);
            kept.unread_change = motion.start(motion::FADE, now).map(|fade| UnreadChange {
                old_box: drawn.subject_w,
                old_accent: drawn.unread,
                fade,
            });
        }
        if starred != kept.starred {
            kept.starred = starred;
            kept.star = motion.start(STAR, now);
        }
        kept.frame(now)
    }

    /// Start a fade when refreshed labels replace a visible row's labels.
    pub fn observe_labels(&mut self, id: &str, labels: &[String], now: Instant) {
        if let Some(kept) = self.rows.get_mut(id)
            && kept.labels.as_deref() != Some(labels)
        {
            kept.previous_labels = kept
                .labels
                .replace(labels.to_vec())
                .zip(self.motion.start(motion::FADE, now));
        }
    }

    pub fn labels_frame(
        &mut self,
        id: &str,
        labels: &[String],
        now: Instant,
    ) -> Option<(Vec<String>, f32)> {
        self.observe_labels(id, labels, now);
        let kept = self.rows.get_mut(id)?;
        if kept
            .previous_labels
            .as_ref()
            .is_some_and(|(_, t)| !t.running(now))
        {
            kept.previous_labels = None;
        }
        kept.previous_labels
            .as_ref()
            .map(|(labels, t)| (labels.clone(), t.progress(now)))
    }

    pub fn observe_attachment(&mut self, id: &str, attached: bool, now: Instant) {
        if let Some(row) = self.rows.get_mut(id) {
            let previous = row.attached.replace(attached);
            if previous == Some(false) && attached {
                row.attachment = self.motion.start(motion::FADE, now);
            } else if !attached {
                row.attachment = None;
            }
        }
    }

    pub fn attachment_alpha(&mut self, id: &str, attached: bool, now: Instant, drawn: bool) -> f32 {
        if drawn {
            self.observe_attachment(id, attached, now);
        }
        self.rows
            .get(id)
            .map_or(1.0, |row| motion::progress(row.attachment.as_ref(), now))
    }

    /// A key arrived: finish running transitions before acting on it.
    pub fn settle(&mut self) {
        for kept in self.rows.values_mut() {
            kept.unread_change = None;
            kept.star = None;
            kept.attachment = None;
            kept.previous_labels = None;
        }
    }

    /// True while a row is moving and needs another frame.
    pub fn running(&self, now: Instant) -> bool {
        self.rows.values().any(|kept| kept.running(now))
    }
}

/// The subject's offset and visible width, and the start of the snippet.
/// The subject stays left-aligned. When the box shrinks, its surplus
/// width briefly opens the gap before the snippet settles.
pub fn line_two(box_width: f32, room: f32, shaped_width: f32) -> (f32, f32, f32) {
    let subject_w = box_width.min(shaped_width);
    let surplus = (box_width - room).max(0.0);
    (0.0, subject_w, subject_w + surplus + SUBJECT_GAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Widths of one subject, as the demo font draws it.
    const BOLD: f32 = 151.4;
    const REGULAR: f32 = 133.2;
    const SUBJECT: &str = "Quarterly plan";

    fn natural(unread: bool) -> f32 {
        if unread { BOLD } else { REGULAR }
    }

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    /// Row `id` drawn at `now`, unread or not, unstarred.
    fn draw(widths: &mut RowWidths, id: &str, unread: bool, now: Instant) -> RowFrame {
        widths.frame(id, SUBJECT, (unread, false), natural, now, true)
    }

    fn snippet_x(frame: RowFrame) -> f32 {
        line_two(frame.subject_w, frame.subject_room, natural(frame.bold)).2
    }

    #[test]
    fn marking_read_slides_the_snippet_while_the_unread_accent_fades() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        let before = draw(&mut widths, "t1", true, t0);
        let key = at(t0, 16);
        let start = draw(&mut widths, "t1", false, key);
        assert_eq!(start.subject_w, before.subject_w);
        assert_eq!(start.unread, 1.0);
        assert!(!start.bold, "the subject fits throughout the slide");
        assert_eq!(start.subject_room, REGULAR);
        assert_eq!(snippet_x(start), snippet_x(before));
        let mid = draw(&mut widths, "t1", false, at(key, 40));
        let narrow = REGULAR + SUBJECT_GAP;
        assert!(mid.subject_w < start.subject_w && mid.subject_w > start.subject_room);
        assert!(narrow < snippet_x(mid) && snippet_x(mid) < snippet_x(start));
        assert!(mid.unread > 0.0 && mid.unread < 0.5);

        let done = draw(&mut widths, "t1", false, at(key, 120));
        assert_eq!(snippet_x(done), narrow);
        assert_eq!(done.unread, 0.0);
        assert!(!widths.running(at(key, 120)));
    }

    #[test]
    fn a_read_row_marked_unread_slides_its_snippet_over_a_slide() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        let read = draw(&mut widths, "t1", false, t0);
        let from = snippet_x(read);
        let to = BOLD + SUBJECT_GAP;
        assert_eq!(from, REGULAR + SUBJECT_GAP);
        // The frame that shows the `U` starts at the regular box width.
        let key = at(t0, 50);
        let start = draw(&mut widths, "t1", true, key);
        assert_eq!(start.subject_w, read.subject_w);
        assert!(start.bold);
        assert_eq!(start.subject_room, BOLD);
        let mut last = snippet_x(start);
        for ms in [16, 33, 50, 66, 83, 100] {
            let x = snippet_x(draw(&mut widths, "t1", true, at(key, ms)));
            assert!(x >= last && x <= to, "{ms} ms: {x}");
            last = x;
        }
        // Ease-out: most of the way in the first half.
        let half =
            snippet_x(widths.frame("t1", SUBJECT, (true, false), natural, at(key, 60), false));
        assert!(half - from > (to - from) * 0.75, "{half}");
        assert_eq!(snippet_x(draw(&mut widths, "t1", true, at(key, 120))), to);
        assert!(!widths.running(at(key, 120)));
    }

    #[test]
    fn the_subject_stays_left_aligned_and_the_snippet_settles_at_the_gap() {
        let mut widths = RowWidths::default();
        let start = Instant::now();
        draw(&mut widths, "t1", false, start);
        for (unread, offset) in [(true, 16), (false, 160)] {
            let key = at(start, offset);
            let from = snippet_x(draw(&mut widths, "t1", !unread, key));
            let to = natural(unread) + SUBJECT_GAP;
            let mut previous = from;
            let mut previous_gap = from - natural(unread);
            for ms in 0..=120 {
                let frame = draw(&mut widths, "t1", unread, at(key, ms));
                assert_eq!(frame.bold, unread, "{ms} ms");
                let (subject_x, visible, snippet) =
                    line_two(frame.subject_w, frame.subject_room, natural(frame.bold));
                assert_eq!(subject_x, 0.0, "{ms} ms");
                let gap = snippet - (subject_x + visible);
                if unread {
                    assert!((gap - SUBJECT_GAP).abs() < 0.01, "{ms} ms: {gap} px");
                } else {
                    assert!(
                        gap <= previous_gap && gap >= SUBJECT_GAP,
                        "{ms} ms: {gap} px"
                    );
                }
                if ms == 0 {
                    assert_eq!(from, snippet);
                } else if unread {
                    assert!(snippet >= previous && snippet <= to, "{ms} ms: {snippet}");
                } else {
                    assert!(snippet <= previous && snippet >= to, "{ms} ms: {snippet}");
                }
                if ms == 60 {
                    let distance = (snippet - from).abs();
                    let total = (to - from).abs();
                    assert!((0.75 * total..total).contains(&distance));
                }
                previous = snippet;
                previous_gap = gap;
            }
            assert_eq!(previous, to);
            assert_eq!(previous_gap, SUBJECT_GAP);
        }
    }

    #[test]
    fn a_key_mid_slide_snaps_it_to_the_end() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        draw(&mut widths, "t1", false, t0);
        draw(&mut widths, "t1", true, at(t0, 16));
        assert!(widths.running(at(t0, 40)));
        widths.settle();
        let frame = draw(&mut widths, "t1", true, at(t0, 40));
        assert_eq!(snippet_x(frame), BOLD + SUBJECT_GAP);
        assert_eq!(frame.unread, 1.0);
        assert!(!widths.running(at(t0, 40)));
    }

    #[test]
    fn labels_change_only_on_already_drawn_rows() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        let original = ["triage".into()];
        assert!(widths.labels_frame("t1", &original, t0).is_none());
        draw(&mut widths, "t1", false, t0);
        assert!(widths.labels_frame("t1", &original, t0).is_none());

        let changed = at(t0, 16);
        assert_eq!(
            widths.labels_frame("t1", &[], changed),
            Some((original.to_vec(), 0.0))
        );
        assert!(widths.running(at(t0, 40)));
        assert!(widths.labels_frame("t1", &[], at(t0, 300)).is_none());

        let mut reduced = RowWidths::new(Motion::new(false));
        draw(&mut reduced, "t1", false, t0);
        assert!(reduced.labels_frame("t1", &original, t0).is_none());
        assert!(reduced.labels_frame("t1", &[], changed).is_none());
        assert!(!reduced.running(changed));
    }

    #[test]
    fn reduced_motion_shows_every_change_on_its_first_frame() {
        let mut widths = RowWidths::new(Motion::new(false));
        let t0 = Instant::now();
        draw(&mut widths, "t1", false, t0);
        let unread = widths.frame("t1", SUBJECT, (true, true), natural, at(t0, 16), true);
        assert_eq!(snippet_x(unread), BOLD + SUBJECT_GAP);
        assert_eq!((unread.unread, unread.star), (1.0, 1.0));
        assert!(!widths.running(at(t0, 16)));
        let read = draw(&mut widths, "t1", false, at(t0, 32));
        assert_eq!(snippet_x(read), REGULAR + SUBJECT_GAP);
        assert_eq!(read.unread, 0.0);
        assert!(!widths.running(at(t0, 32)));
    }

    #[test]
    fn every_row_of_a_range_moves_together() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        for id in ["a", "b", "c"] {
            draw(&mut widths, id, false, t0);
        }
        let frame = at(t0, 16);
        for id in ["a", "b", "c"] {
            draw(&mut widths, id, true, frame);
        }
        let xs: Vec<f32> = ["a", "b", "c"]
            .iter()
            .map(|id| snippet_x(draw(&mut widths, id, true, at(frame, 40))))
            .collect();
        assert!(xs.windows(2).all(|pair| pair[0] == pair[1]), "{xs:?}");
        assert!(xs[0] > REGULAR + SUBJECT_GAP);
    }

    #[test]
    fn a_read_toggle_during_a_slide_resumes_at_the_drawn_width() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        draw(&mut widths, "t1", true, t0);
        let key = at(t0, 16);
        draw(&mut widths, "t1", false, key);
        let mid = draw(&mut widths, "t1", false, at(key, 60));
        let reversed = draw(&mut widths, "t1", true, at(key, 60));
        assert_eq!(reversed.subject_w, mid.subject_w);
        assert_eq!(snippet_x(reversed), snippet_x(mid));
        assert_eq!(reversed.unread, mid.unread);
        assert!(reversed.bold);
        assert_eq!(reversed.subject_room, BOLD);
        assert!(draw(&mut widths, "t1", true, at(key, 180)).bold);
    }

    #[test]
    fn the_star_fades_and_scales_in_and_out() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        let star = |widths: &mut RowWidths, on: bool, ms: u64| {
            widths.frame("t1", SUBJECT, (false, on), natural, at(t0, ms), true)
        };
        assert_eq!(star(&mut widths, false, 0).star, 0.0);
        let start = star(&mut widths, true, 16);
        assert_eq!((start.star, start.star_scale()), (0.0, STAR_FROM));
        let mid = star(&mut widths, true, 66);
        assert!(mid.star > 0.5 && mid.star < 1.0, "{}", mid.star);
        assert!(mid.star_scale() > STAR_FROM && mid.star_scale() < 1.0);
        let done = star(&mut widths, true, 116);
        assert_eq!((done.star, done.star_scale()), (1.0, 1.0));
        // Unstar: fades out the same way.
        star(&mut widths, false, 200);
        assert!(star(&mut widths, false, 250).star < 0.5);
        assert_eq!(star(&mut widths, false, 300).star, 0.0);
    }

    #[test]
    fn subject_measurement_uses_the_rows_weight_and_the_same_gap() {
        use std::cell::Cell;

        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        for (id, unread) in [("read", false), ("unread", true)] {
            let measured = Cell::new(None);
            let frame = widths.frame(
                id,
                SUBJECT,
                (unread, false),
                |bold| {
                    measured.set(Some(bold));
                    natural(bold)
                },
                t0,
                true,
            );
            assert_eq!(measured.get(), Some(unread));
            assert_eq!(frame.bold, unread);
            assert_eq!(frame.subject_w, natural(unread));
            assert_eq!(snippet_x(frame) - natural(unread), SUBJECT_GAP);
        }
    }

    #[test]
    fn a_row_that_scrolls_off_lays_out_at_its_own_weight_again() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        let ids = ["t0", "t1", "t2"];
        draw(&mut widths, "t1", true, t0);
        let start = draw(&mut widths, "t1", false, at(t0, 16));
        assert_eq!(snippet_x(start), BOLD + SUBJECT_GAP);
        widths.keep_drawn(2..3, ids.iter().copied());
        let back = draw(&mut widths, "t1", false, at(t0, 32));
        assert_eq!(snippet_x(back), REGULAR + SUBJECT_GAP);
    }

    #[test]
    fn a_measuring_pass_records_and_starts_nothing() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        widths.frame("t0", SUBJECT, (false, false), natural, t0, false);
        // First drawn unread: laid out bold, nothing moving.
        let frame = draw(&mut widths, "t0", true, at(t0, 16));
        assert_eq!(snippet_x(frame), BOLD + SUBJECT_GAP);
        assert!(!widths.running(at(t0, 16)));
        // A measuring pass sees a change but does not start it.
        let measured = widths.frame("t0", SUBJECT, (false, false), natural, at(t0, 32), false);
        assert_eq!(measured.unread, 1.0);
        assert!(!widths.running(at(t0, 32)));
    }

    #[test]
    fn a_new_folder_or_new_subject_starts_over() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        draw(&mut widths, "t1", true, t0);
        widths.clear();
        let frame = draw(&mut widths, "t1", false, at(t0, 16));
        assert_eq!(snippet_x(frame), REGULAR + SUBJECT_GAP);
        assert_eq!(frame.unread, 0.0);
        // A row whose subject changed lays out for the new text, at once.
        let other = widths.frame("t1", "Other", (true, false), |_| 40.0, at(t0, 32), true);
        assert_eq!((other.subject_w, other.unread), (40.0, 1.0));
    }
}
