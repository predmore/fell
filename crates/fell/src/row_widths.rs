//! How a thread row lays out its subject, and how it moves when the row
//! turns read, unread, starred or unstarred.
//!
//! A row is as wide as its text at its own weight, so read and unread rows
//! at rest leave the same gap after the subject. A row keeps the width it
//! had on screen while its weight changes, so opening it moves nothing.
//! What changes is drawn over [`crate::motion`]'s durations, and starts
//! from what the last frame drew, as [`crate::inbox_motion`] does, so
//! open, `U`, `s`, undo, an agent and sync all move a row the same way, and
//! every row a `V` range changes moves together:
//! - the unread bar fades over [`motion::FADE`]. The text stays legible
//!   at one weight throughout and changes weight once the box has settled;
//! - a read row marked unread grows its subject box to fit the bold text,
//!   and the snippet slides there ([`motion::SLIDE`]) instead of the
//!   subject being cut short;
//! - the star fades and scales in from [`STAR_FROM`] ([`STAR`]);
//! - a box left wider than its text (a row opened, so now read) stays
//!   until the reader next moves in the list: the key that brings the list
//!   back or moves its cursor ([`RowWidths::relax`]) eases it back to its
//!   text ([`RELAX`]), in the frame that key's own transition starts in.
//!   Nothing moves on a timer while the reader reads.
//!
//! With motion reduced every change shows on its first frame, the relax
//! too: on that key, at once.

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

/// How long a box wider than its text takes to ease back: as long as the
/// pane slide or list glide of the key that starts it.
pub const RELAX: Duration = motion::PANE;

/// How wide the fade is that softens the snippet's right edge while a
/// relaxing box carries it past the end of the line.
pub const EDGE_FADE: f32 = 16.0;

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
        let subject_w = subject_w.ceil();
        Self {
            subject_w,
            subject_room: subject_w,
            unread: f32::from(u8::from(unread)),
            bold: unread,
            star: f32::from(u8::from(starred)),
        }
    }

    /// How strongly the snippet's right edge fades this frame: while a
    /// relaxing box (wider than where it is going) carries the snippet,
    /// cut for where it is going, past the end of the line. 0 at rest.
    pub fn edge_fade(&self) -> f32 {
        ((self.subject_w - self.subject_room) / (EDGE_FADE / 2.0)).clamp(0.0, 1.0)
    }

    /// The star's scale this frame.
    pub fn star_scale(&self) -> f32 {
        motion::lerp(STAR_FROM, 1.0, self.star)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Kept {
    /// The subject the widths belong to.
    subject: String,
    /// The box width at rest: where it is, or where it is easing to.
    width: f32,
    /// The subject's own width at the row's current weight.
    natural: f32,
    /// The box easing from this width to `width`.
    slide: Option<(f32, Transition)>,
    unread: bool,
    starred: bool,
    /// Starting unread accent and text weight while the accent eases.
    unread_transition: Option<(f32, bool, Transition)>,
    /// The star fading in (`starred`) or out.
    star: Option<Transition>,
    /// The reader moved in the list: ease back to the text on the next
    /// frame that draws the row.
    relax: bool,
}

impl Kept {
    fn frame(&self, now: Instant) -> RowFrame {
        let subject_w = match &self.slide {
            Some((from, slide)) => motion::lerp(*from, self.width, slide.progress(now)),
            None => self.width,
        };
        let toward = |on: bool, transition: Option<&Transition>| {
            let t = motion::progress(transition, now);
            if on { t } else { 1.0 - t }
        };
        let unread_target = f32::from(u8::from(self.unread));
        let (unread, bold) = match self.unread_transition {
            Some((from, bold, transition)) if transition.running(now) => (
                motion::lerp(from, unread_target, transition.progress(now)),
                bold,
            ),
            _ => (unread_target, self.unread),
        };
        RowFrame {
            subject_w,
            subject_room: self.width,
            unread,
            bold,
            star: toward(self.starred, self.star.as_ref()),
        }
    }

    fn running(&self, now: Instant) -> bool {
        self.slide.is_some_and(|(_, t)| t.running(now))
            || self
                .unread_transition
                .is_some_and(|(_, _, t)| t.running(now))
            || self.star.is_some_and(|t| t.running(now))
    }

    /// Wider than its text: it eases back when the reader next moves.
    fn wide(&self) -> bool {
        self.width > self.natural + 0.5
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
            let width = natural(unread).ceil();
            if record {
                self.rows.insert(
                    id.to_string(),
                    Kept {
                        subject: subject.to_string(),
                        width,
                        natural: width,
                        slide: None,
                        unread,
                        starred,
                        unread_transition: None,
                        star: None,
                        relax: false,
                    },
                );
            }
            return RowFrame::still(width, unread, starred);
        };
        if !record {
            return kept.frame(now);
        }
        if kept.slide.is_some_and(|(_, t)| !t.running(now)) {
            kept.slide = None;
        }
        if kept
            .unread_transition
            .is_some_and(|(_, _, t)| !t.running(now))
        {
            kept.unread_transition = None;
        }
        if kept.star.is_some_and(|t| !t.running(now)) {
            kept.star = None;
        }
        if unread != kept.unread {
            let drawn = kept.frame(now);
            kept.unread = unread;
            kept.unread_transition = motion
                .start(motion::FADE, now)
                .map(|transition| (drawn.unread, drawn.bold, transition));
            kept.natural = natural(unread).ceil();
            // Grow to fit rather than cut the text short (a read row marked
            // unread). A box that is wider stays for now, so a row that
            // turns read does not move under the reader.
            if kept.natural > kept.width {
                kept.width = kept.natural;
                kept.slide = motion
                    .start(motion::SLIDE, now)
                    .map(|slide| (drawn.subject_w, slide));
            }
        }
        if starred != kept.starred {
            kept.starred = starred;
            kept.star = motion.start(STAR, now);
        }
        if std::mem::take(&mut kept.relax) && kept.wide() {
            // Eases from where it is drawn, or lands at once with motion
            // reduced.
            let drawn = kept.frame(now).subject_w;
            kept.slide = motion.start(RELAX, now).map(|relax| (drawn, relax));
            kept.width = kept.natural;
        }
        kept.frame(now)
    }

    /// The reader moved in the list (its cursor, or back to it): every box
    /// left wider than its text eases back on the next frame, with what
    /// that key moves.
    pub fn relax(&mut self) {
        for kept in self.rows.values_mut() {
            kept.relax = kept.wide();
        }
    }

    /// A key arrived: every row jumps to its end state. A box left wide
    /// stays until [`RowWidths::relax`].
    pub fn settle(&mut self) {
        for kept in self.rows.values_mut() {
            kept.slide = None;
            kept.unread_transition = None;
            kept.star = None;
        }
    }

    /// True while a row is moving and needs another frame.
    pub fn running(&self, now: Instant) -> bool {
        self.rows.values().any(|kept| kept.running(now))
    }
}

/// Line two of a row: the subject box width and where the snippet starts,
/// for a subject box of `subject` px in a line `text_width` px wide.
pub fn line_two(subject: f32, text_width: f32) -> (f32, f32) {
    let subject_w = subject.min(SUBJECT_MAX).min(text_width);
    (subject_w, subject_w + SUBJECT_GAP)
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
        line_two(frame.subject_w, 400.0).1
    }

    #[test]
    fn opening_an_unread_row_moves_nothing_and_fades_the_accent() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        let before = draw(&mut widths, "t1", true, t0);
        assert_eq!(before.unread, 1.0);
        // Opened: read on the next frame.
        let first = draw(&mut widths, "t1", false, at(t0, 16));
        assert_eq!(snippet_x(first), snippet_x(before));
        assert_eq!(
            first.unread, 1.0,
            "the fade starts on the frame that shows it"
        );
        let mid = draw(&mut widths, "t1", false, at(t0, 16 + 60));
        assert!(mid.unread > 0.0 && mid.unread < 0.5, "{}", mid.unread);
        let done = draw(&mut widths, "t1", false, at(t0, 16 + 120));
        assert_eq!(done.unread, 0.0);
        assert_eq!(snippet_x(done), BOLD.ceil() + SUBJECT_GAP);
        // `U` again: back to bold, still in place.
        let again = draw(&mut widths, "t1", true, at(t0, 300));
        assert_eq!(snippet_x(again), snippet_x(before));
    }

    #[test]
    fn a_read_row_marked_unread_slides_its_snippet_over_a_slide() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        let read = draw(&mut widths, "t1", false, t0);
        let from = snippet_x(read);
        let to = BOLD.ceil() + SUBJECT_GAP;
        assert_eq!(from, REGULAR.ceil() + SUBJECT_GAP);
        // The frame that shows the `U` starts the slide where it was.
        let key = at(t0, 50);
        let start = draw(&mut widths, "t1", true, key);
        assert_eq!(snippet_x(start), from);
        // The bold text is cut to where the box is going, not to the box.
        assert_eq!(start.subject_room, BOLD.ceil());
        let mut last = from;
        for ms in [16, 33, 50, 66, 83, 100] {
            let x = snippet_x(draw(&mut widths, "t1", true, at(key, ms)));
            assert!(x > last && x < to, "{ms} ms: {x}");
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
    fn a_key_mid_slide_snaps_it_to_the_end() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        draw(&mut widths, "t1", false, t0);
        draw(&mut widths, "t1", true, at(t0, 16));
        assert!(widths.running(at(t0, 40)));
        widths.settle();
        let frame = draw(&mut widths, "t1", true, at(t0, 40));
        assert_eq!(snippet_x(frame), BOLD.ceil() + SUBJECT_GAP);
        assert_eq!(frame.unread, 1.0);
        assert!(!widths.running(at(t0, 40)));
    }

    #[test]
    fn reduced_motion_shows_every_change_on_its_first_frame() {
        let mut widths = RowWidths::new(Motion::new(false));
        let t0 = Instant::now();
        draw(&mut widths, "t1", false, t0);
        let unread = widths.frame("t1", SUBJECT, (true, true), natural, at(t0, 16), true);
        assert_eq!(snippet_x(unread), BOLD.ceil() + SUBJECT_GAP);
        assert_eq!((unread.unread, unread.star), (1.0, 1.0));
        assert!(!widths.running(at(t0, 16)));
        // Read again: the wider box stays, and never relaxes on its own.
        draw(&mut widths, "t1", false, at(t0, 32));
        let later = draw(&mut widths, "t1", false, at(t0, 5_000));
        assert_eq!(snippet_x(later), BOLD.ceil() + SUBJECT_GAP);
        // The reader moves: it lands on its text on that key's frame.
        widths.relax();
        let moved = draw(&mut widths, "t1", false, at(t0, 5_016));
        assert_eq!(snippet_x(moved), REGULAR.ceil() + SUBJECT_GAP);
        assert!(!widths.running(at(t0, 5_016)));
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
        assert!(xs[0] > REGULAR.ceil() + SUBJECT_GAP);
    }

    #[test]
    fn a_wide_box_waits_for_the_reader_to_move_then_eases_back() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        draw(&mut widths, "t1", true, t0);
        // Opened at 16 ms: the box stays bold-wide however long it rests.
        draw(&mut widths, "t1", false, at(t0, 16));
        let wide = BOLD.ceil() + SUBJECT_GAP;
        let narrow = REGULAR.ceil() + SUBJECT_GAP;
        for ms in [400, 700, 10_000] {
            assert_eq!(snippet_x(draw(&mut widths, "t1", false, at(t0, ms))), wide);
            assert!(!widths.running(at(t0, ms)), "{ms} ms");
        }
        // `j` (or Esc back to the list): eases from the frame that shows it.
        widths.settle();
        widths.relax();
        let key = at(t0, 10_016);
        let start = draw(&mut widths, "t1", false, key);
        assert_eq!(snippet_x(start), wide);
        assert!(widths.running(key));
        // The snippet is cut for where it is going, so its right edge fades
        // while it is carried past the end of the line.
        assert_eq!(start.subject_room, REGULAR.ceil());
        assert_eq!(start.edge_fade(), 1.0);
        let mid = draw(&mut widths, "t1", false, at(key, 40));
        assert!(snippet_x(mid) < wide && snippet_x(mid) > narrow);
        // Ease-out: most of the way after a quarter of the time.
        assert!(wide - snippet_x(mid) > (wide - narrow) * 0.5, "{mid:?}");
        let done = draw(&mut widths, "t1", false, at(key, 160));
        assert_eq!(snippet_x(done), narrow);
        assert_eq!(done.edge_fade(), 0.0);
        assert!(!widths.running(at(key, 160)));
        // Once relaxed, another move changes nothing.
        widths.relax();
        assert_eq!(
            snippet_x(draw(&mut widths, "t1", false, at(key, 200))),
            narrow
        );
        assert!(!widths.running(at(key, 200)));
    }

    #[test]
    fn a_move_leaves_rows_at_their_text_alone() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        draw(&mut widths, "read", false, t0);
        draw(&mut widths, "unread", true, t0);
        widths.relax();
        for id in ["read", "unread"] {
            draw(&mut widths, id, id == "unread", at(t0, 16));
        }
        assert!(!widths.running(at(t0, 16)));
    }

    #[test]
    fn text_keeps_one_weight_through_the_accent_fade() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        draw(&mut widths, "t1", true, t0);
        let key = at(t0, 16);
        assert!(draw(&mut widths, "t1", false, key).bold);
        let mid = draw(&mut widths, "t1", false, at(key, 60));
        assert!(mid.bold && mid.unread > 0.0 && mid.unread < 1.0);
        let reversed = draw(&mut widths, "t1", true, at(key, 60));
        assert_eq!(reversed, mid, "a second update starts from the drawn state");
        assert!(draw(&mut widths, "t1", true, at(key, 180)).bold);
        let second = at(key, 220);
        assert!(draw(&mut widths, "t1", false, second).bold);
        assert!(!draw(&mut widths, "t1", false, at(second, 120)).bold);
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
    fn an_untouched_read_row_uses_its_own_width() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        draw(&mut widths, "unread", true, t0);
        let read = snippet_x(draw(&mut widths, "read", false, t0));
        assert_eq!(read, REGULAR.ceil() + SUBJECT_GAP);
        // The gap after the subject is the same on both kinds of row.
        assert!(read - REGULAR < SUBJECT_GAP + 1.0);
    }

    #[test]
    fn a_row_that_scrolls_off_lays_out_at_its_own_weight_again() {
        let mut widths = RowWidths::new(Motion::new(false));
        let t0 = Instant::now();
        let ids = ["t0", "t1", "t2"];
        draw(&mut widths, "t1", true, t0);
        // Opened: still the unread width while on screen.
        widths.keep_drawn(0..3, ids.iter().copied());
        let opened = draw(&mut widths, "t1", false, at(t0, 16));
        assert_eq!(snippet_x(opened), BOLD.ceil() + SUBJECT_GAP);
        // Scrolled so only t2 is drawn, then back.
        widths.keep_drawn(2..3, ids.iter().copied());
        let back = draw(&mut widths, "t1", false, at(t0, 32));
        assert_eq!(snippet_x(back), REGULAR.ceil() + SUBJECT_GAP);
    }

    #[test]
    fn a_measuring_pass_records_and_starts_nothing() {
        let mut widths = RowWidths::default();
        let t0 = Instant::now();
        widths.frame("t0", SUBJECT, (false, false), natural, t0, false);
        // First drawn unread: laid out bold, nothing moving.
        let frame = draw(&mut widths, "t0", true, at(t0, 16));
        assert_eq!(snippet_x(frame), BOLD.ceil() + SUBJECT_GAP);
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
        assert_eq!(snippet_x(frame), REGULAR.ceil() + SUBJECT_GAP);
        assert_eq!(frame.unread, 0.0);
        // A row whose subject changed lays out for the new text, at once.
        let other = widths.frame("t1", "Other", (true, false), |_| 40.0, at(t0, 32), true);
        assert_eq!((other.subject_w, other.unread), (40.0, 1.0));
    }
}
