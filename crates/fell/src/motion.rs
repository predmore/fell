//! Motion: the quick transitions docs/ux.md asks for, shared by every view.
//!
//! A [`Transition`] is a start time and a duration. A view keeps the ones it
//! is running, reads their eased progress while it renders, and asks GPUI for
//! another frame while any of them runs ([`gpui::Window::request_animation_frame`]).
//! Nothing here owns a timer or a thread: the frames that draw an animation
//! are the only work it costs.
//!
//! The rules every caller follows:
//! - Input never waits. A key press settles every running transition first
//!   (jumps it to its end state) and then acts, so a key during a transition
//!   behaves exactly as it would after it.
//! - The state the user acts on changes at once. Only what is drawn trails
//!   behind: an archived row is gone from the list and the next row is
//!   selected in the same frame, and only its picture collapses.
//! - [`Motion::start`] returns `None` when motion is reduced, so a reduced
//!   setting draws every end state on the first frame with no extra code.
//!
//! Durations are named for what they do and sit in the 80–160 ms range with
//! an ease-out curve: fast enough to never feel like waiting, long enough to
//! show where something came from or went.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A colour or focus change: a pane's focus edge.
pub const QUICK: Duration = Duration::from_millis(80);
/// A row leaving the list: it fades while the rows below close the gap.
pub const COLLAPSE: Duration = Duration::from_millis(100);
/// Rows arriving or moving, a pane sliding in, a scroll gliding to its target.
pub const SLIDE: Duration = Duration::from_millis(120);
/// A pane coming into view: the conversation replacing the list in a
/// narrow window, or the list coming back. The longest move, so the longest.
pub const PANE: Duration = Duration::from_millis(160);
/// An overlay (palette, folder picker, which-key) or a notice fading in.
pub const FADE: Duration = Duration::from_millis(120);
/// A notice fading out after the next key cleared it.
pub const FADE_OUT: Duration = Duration::from_millis(160);

/// How far an overlay rises while it fades in, in pixels.
pub const RISE_PX: f32 = 6.0;
/// How far a pane that comes into view slides in from its side, in pixels.
pub const PANE_SLIDE_PX: f32 = 24.0;
/// An overlay opens at this fraction of its width and grows to full size.
pub const OPEN_SCALE: f32 = 0.96;
/// Another thread's messages arrive at this opacity and settle to full,
/// so switching threads softens instead of blinking through empty.
pub const ARRIVE_FROM: f32 = 0.4;
/// A glide never starts further than this many viewports from its target:
/// `gg` from far down the list covers the last stretch, not every row.
pub const GLIDE_MAX_VIEWPORTS: f32 = 1.0;

/// Ease-out cubic: fast start, gentle stop.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// Round `value` (logical pixels) to whole device pixels at `scale`.
pub fn snap(value: f32, scale: f32) -> f32 {
    if scale <= 0.0 {
        return value.round();
    }
    (value * scale).round() / scale
}

/// Blend two `0xRRGGBB` colours: `t = 0` is `from`, `t = 1` is `to`.
pub fn mix(from: u32, to: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xff) as f32;
        let b = ((to >> shift) & 0xff) as f32;
        (lerp(a, b, t).round() as u32).min(0xff) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// One running transition. Its progress is time-based, so a slow frame
/// skips ahead instead of stretching the animation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transition {
    started: Instant,
    duration: Duration,
}

impl Transition {
    pub fn new(started: Instant, duration: Duration) -> Self {
        Self { started, duration }
    }

    /// Linear progress from 0 to 1.
    pub fn linear(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.started);
        (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }

    /// Eased progress from 0 to 1.
    pub fn progress(&self, now: Instant) -> f32 {
        ease_out(self.linear(now))
    }

    pub fn running(&self, now: Instant) -> bool {
        self.linear(now) < 1.0
    }
}

/// Eased progress of an optional transition: 1 (the end state) when there is none.
pub fn progress(transition: Option<&Transition>, now: Instant) -> f32 {
    transition.map_or(1.0, |transition| transition.progress(now))
}

/// Whether motion is on, read once at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Motion {
    enabled: bool,
}

impl Motion {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// `FELL_REDUCE_MOTION=1` turns motion off and `=0` forces it on.
    /// Otherwise the desktop decides: `gtk-enable-animations=false` in the
    /// GTK settings file is the reduced-motion switch GNOME, GTK apps and
    /// Omarchy's GTK config share.
    pub fn from_env() -> Self {
        let reduce = match env::var("FELL_REDUCE_MOTION") {
            Ok(value) => is_true(&value),
            Err(_) => desktop_reduces_motion(gtk_settings_paths()),
        };
        Self::new(!reduce)
    }

    /// A transition starting now, or `None` when motion is reduced.
    pub fn start(&self, duration: Duration, now: Instant) -> Option<Transition> {
        self.enabled.then(|| Transition::new(now, duration))
    }
}

fn is_true(value: &str) -> bool {
    let value = value.trim();
    !(value.is_empty()
        || value == "0"
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("off"))
}

fn gtk_settings_paths() -> Vec<PathBuf> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    config
        .map(|config| {
            vec![
                config.join("gtk-4.0/settings.ini"),
                config.join("gtk-3.0/settings.ini"),
            ]
        })
        .unwrap_or_default()
}

/// The first settings file that names `gtk-enable-animations` decides.
fn desktop_reduces_motion(paths: Vec<PathBuf>) -> bool {
    paths
        .into_iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .find_map(|text| animations_setting(&text))
        .is_some_and(|enabled| !enabled)
}

fn animations_setting(ini: &str) -> Option<bool> {
    ini.lines().find_map(|line| {
        let line = line.trim().replace(' ', "");
        let value = line.strip_prefix("gtk-enable-animations=")?;
        Some(is_true(value))
    })
}

/// A value travelling from `from` to `to`: a scroll offset gliding to its target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glide {
    pub from: f32,
    pub to: f32,
    transition: Transition,
}

impl Glide {
    /// A glide from `from` to `to`, or `None` when there is nothing to
    /// travel or motion is reduced. A start further than `reach` from the
    /// target is pulled in to `reach`, so a long jump covers the last
    /// stretch at the same speed as a short one.
    pub fn start(
        motion: &Motion,
        from: f32,
        to: f32,
        reach: f32,
        duration: Duration,
        now: Instant,
    ) -> Option<Glide> {
        if (to - from).abs() < 0.5 {
            return None;
        }
        let from = if reach > 0.0 {
            to + (from - to).clamp(-reach, reach)
        } else {
            from
        };
        Some(Glide {
            from,
            to,
            transition: motion.start(duration, now)?,
        })
    }

    pub fn value(&self, now: Instant) -> f32 {
        lerp(self.from, self.to, self.transition.progress(now))
    }

    pub fn running(&self, now: Instant) -> bool {
        self.transition.running(now)
    }
}

/// How one row of a list is drawn while the list changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowMotion {
    /// Draw the row this far below its slot (negative: above).
    pub dy: f32,
    pub opacity: f32,
}

impl RowMotion {
    pub const STILL: RowMotion = RowMotion {
        dy: 0.0,
        opacity: 1.0,
    };
}

/// A row that left the list, drawn where it was while it fades out and the
/// rows around it close the gap.
#[derive(Clone, Debug)]
pub struct Ghost<R> {
    pub row: R,
    /// Its top before it left, in viewport pixels.
    pub top: f32,
    transition: Transition,
}

impl<R> Ghost<R> {
    pub fn opacity(&self, now: Instant) -> f32 {
        1.0 - self.transition.progress(now)
    }

    pub fn running(&self, now: Instant) -> bool {
        self.transition.running(now)
    }
}

/// Where the rows of a fixed-height list were before a change, and how
/// they travel to where they are now (FLIP: first, last, invert, play).
///
/// The list itself changes at once. Each row is drawn offset by where it
/// used to be, and the offset eases to zero: rows below an archived row
/// slide up into its place, rows below an undone row slide down to make
/// room, and a row that was not on screen fades in. Rows that left are
/// [`Ghost`]s. When no row on screen survives the change (another folder,
/// a jump to the far end), the new rows fade in and nothing slides.
#[derive(Clone, Debug)]
pub struct ListShift<R> {
    transition: Transition,
    /// Row id → its top before the change, in viewport pixels.
    before: HashMap<String, f32>,
    /// Row id → how far it is drawn from its slot when the shift starts,
    /// fixed the first frame the row is drawn. `None`: the row is new.
    resolved: HashMap<String, Option<f32>>,
    ghosts: Vec<Ghost<R>>,
    replaced: bool,
    /// Furthest a row slides; one further away fades in instead.
    reach: f32,
}

impl<R: Clone> ListShift<R> {
    /// `before`: id and viewport top of each row drawn before the change
    /// (and a few beyond the edges, so rows sliding in from just outside
    /// slide instead of fading). `left`: rows among them that are gone.
    /// `carried`: ghosts of an earlier shift still fading, kept so a quick
    /// second change does not cut them off.
    pub fn start(
        motion: &Motion,
        before: Vec<(String, f32)>,
        left: Vec<(R, f32)>,
        carried: Vec<Ghost<R>>,
        reach: f32,
        now: Instant,
    ) -> Option<ListShift<R>> {
        let transition = motion.start(SLIDE, now)?;
        let replaced = !before.is_empty() && left.len() == before.len();
        let fade = motion.start(COLLAPSE, now)?;
        let mut ghosts: Vec<Ghost<R>> = carried
            .into_iter()
            .filter(|ghost| ghost.running(now))
            .collect();
        if !replaced {
            ghosts.extend(left.into_iter().map(|(row, top)| Ghost {
                row,
                top,
                transition: fade,
            }));
        }
        Some(ListShift {
            transition,
            before: before.into_iter().collect(),
            resolved: HashMap::new(),
            ghosts,
            replaced,
            reach,
        })
    }

    /// How to draw row `id`, whose slot now starts at `top` (viewport pixels).
    pub fn row(&mut self, id: &str, top: f32, now: Instant) -> RowMotion {
        let p = self.transition.progress(now);
        let replaced = self.replaced;
        let reach = self.reach;
        let before = &self.before;
        let start = *self.resolved.entry(id.to_string()).or_insert_with(|| {
            if replaced {
                return None;
            }
            match before.get(id) {
                Some(old) if (old - top).abs() <= reach => Some(old - top),
                _ => None,
            }
        });
        match start {
            Some(dy) => RowMotion {
                dy: dy * (1.0 - p),
                opacity: 1.0,
            },
            None => RowMotion {
                dy: 0.0,
                opacity: p,
            },
        }
    }

    /// How far row `id` is drawn from its slot right now: an interrupted
    /// shift hands this to the next one, so a row in flight keeps its place.
    pub fn offset_of(&self, id: &str, now: Instant) -> f32 {
        let p = self.transition.progress(now);
        match self.resolved.get(id) {
            Some(Some(dy)) => dy * (1.0 - p),
            _ => 0.0,
        }
    }

    /// Ghosts whose old top falls in the slot `[top, top + height)`, with
    /// their top relative to that slot. The view draws each beneath the
    /// row in that slot, so the row slides over it as it fades.
    pub fn ghosts_in(&self, top: f32, height: f32, now: Instant) -> Vec<(R, f32, f32)> {
        self.ghosts
            .iter()
            .filter(|ghost| ghost.running(now) && ghost.top >= top && ghost.top < top + height)
            .map(|ghost| (ghost.row.clone(), ghost.top - top, ghost.opacity(now)))
            .collect()
    }

    pub fn take_ghosts(&mut self) -> Vec<Ghost<R>> {
        std::mem::take(&mut self.ghosts)
    }

    pub fn running(&self, now: Instant) -> bool {
        self.transition.running(now) || self.ghosts.iter().any(|ghost| ghost.running(now))
    }

    /// Every row drawn so far sits in its slot and nothing fades: the
    /// change moved nothing on screen, so no more frames are needed.
    pub fn is_still(&self) -> bool {
        !self.resolved.is_empty()
            && self.ghosts.is_empty()
            && self
                .resolved
                .values()
                .all(|start| matches!(start, Some(dy) if dy.abs() < 0.5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn durations_stay_in_the_quick_range() {
        for duration in [QUICK, COLLAPSE, SLIDE, PANE, FADE, FADE_OUT] {
            assert!((80..=160).contains(&duration.as_millis()), "{duration:?}");
        }
    }

    #[test]
    fn ease_out_starts_fast_and_lands_exactly() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert!(ease_out(0.25) > 0.25, "ease-out leads the clock");
        assert_eq!(ease_out(2.0), 1.0);
        assert_eq!(ease_out(-1.0), 0.0);
    }

    #[test]
    fn a_transition_is_time_based_and_ends() {
        let start = Instant::now();
        let transition = Transition::new(start, SLIDE);
        assert_eq!(transition.progress(start), 0.0);
        assert!(transition.running(at(start, 60)));
        assert!(transition.progress(at(start, 60)) > 0.5);
        assert_eq!(transition.progress(at(start, 120)), 1.0);
        assert!(!transition.running(at(start, 120)));
        assert_eq!(transition.progress(at(start, 900)), 1.0);
    }

    #[test]
    fn reduced_motion_starts_nothing() {
        let off = Motion::new(false);
        let now = Instant::now();
        assert!(off.start(SLIDE, now).is_none());
        assert!(Glide::start(&off, 0.0, -400.0, 0.0, SLIDE, now).is_none());
        let shift: Option<ListShift<u8>> =
            ListShift::start(&off, vec![("a".into(), 0.0)], vec![], vec![], 600.0, now);
        assert!(shift.is_none());
        assert_eq!(progress(None, now), 1.0);
    }

    #[test]
    fn reduce_motion_reads_env_values_and_gtk_settings() {
        assert!(is_true("1"));
        assert!(is_true("true"));
        assert!(!is_true("0"));
        assert!(!is_true("off"));
        assert_eq!(
            animations_setting("[Settings]\ngtk-enable-animations = false\n"),
            Some(false)
        );
        assert_eq!(animations_setting("gtk-enable-animations=1"), Some(true));
        assert_eq!(animations_setting("gtk-theme-name=Adwaita"), None);
    }

    #[test]
    fn mix_blends_each_channel() {
        assert_eq!(mix(0x000000, 0xffffff, 0.0), 0x000000);
        assert_eq!(mix(0x000000, 0xffffff, 1.0), 0xffffff);
        assert_eq!(mix(0x000000, 0x2040ff, 0.5), 0x102080);
    }

    #[test]
    fn a_glide_lands_on_its_target_and_caps_its_reach() {
        let motion = Motion::new(true);
        let start = Instant::now();
        let glide = Glide::start(&motion, 0.0, -3000.0, 600.0, SLIDE, start).unwrap();
        assert_eq!(glide.from, -2400.0, "starts one viewport away");
        assert_eq!(glide.value(at(start, 120)), -3000.0);
        assert!(glide.value(at(start, 40)) < -2400.0);
        assert!(Glide::start(&motion, -10.0, -10.2, 600.0, SLIDE, start).is_none());
    }

    #[test]
    fn rows_below_an_archived_row_slide_up_over_its_ghost() {
        let motion = Motion::new(true);
        let start = Instant::now();
        // Rows a, b, c at 0, 68, 136. b is archived: c now sits at 68.
        let before = vec![("a".into(), 0.0), ("b".into(), 68.0), ("c".into(), 136.0)];
        let mut shift =
            ListShift::start(&motion, before, vec![("b", 68.0)], vec![], 600.0, start).unwrap();
        assert_eq!(shift.row("a", 0.0, start), RowMotion::STILL);
        let c = shift.row("c", 68.0, start);
        assert_eq!(
            c,
            RowMotion {
                dy: 68.0,
                opacity: 1.0
            },
            "c starts where it was"
        );
        let ghosts = shift.ghosts_in(68.0, 68.0, start);
        assert_eq!(ghosts.len(), 1);
        assert_eq!(ghosts[0].0, "b");
        assert_eq!(ghosts[0].1, 0.0);
        assert!(shift.running(at(start, 60)));
        let mid = shift.row("c", 68.0, at(start, 60));
        assert!(mid.dy > 0.0 && mid.dy < 68.0);
        assert_eq!(shift.row("c", 68.0, at(start, 120)), RowMotion::STILL);
        assert!(shift.ghosts_in(68.0, 68.0, at(start, 120)).is_empty());
        assert!(!shift.running(at(start, 120)));
    }

    #[test]
    fn a_new_row_fades_in_and_a_far_row_does_not_fly() {
        let motion = Motion::new(true);
        let start = Instant::now();
        let before = vec![("a".into(), 0.0), ("far".into(), 0.0)];
        let mut shift: ListShift<&str> =
            ListShift::start(&motion, before, vec![], vec![], 300.0, start).unwrap();
        assert_eq!(shift.row("new", 0.0, start).opacity, 0.0);
        assert_eq!(
            shift.row("far", 900.0, start),
            RowMotion {
                dy: 0.0,
                opacity: 0.0
            }
        );
        assert_eq!(shift.row("new", 0.0, at(start, 120)).opacity, 1.0);
    }

    #[test]
    fn a_replaced_list_fades_in_without_ghosts() {
        let motion = Motion::new(true);
        let start = Instant::now();
        let before = vec![("a".into(), 0.0)];
        let mut shift =
            ListShift::start(&motion, before, vec![("a", 0.0)], vec![], 600.0, start).unwrap();
        assert!(shift.ghosts_in(0.0, 68.0, start).is_empty());
        assert_eq!(shift.row("x", 0.0, start).opacity, 0.0);
    }

    #[test]
    fn a_change_that_moves_nothing_is_still() {
        let motion = Motion::new(true);
        let start = Instant::now();
        // New mail above the viewport: the list scrolled by the same amount.
        let before = vec![("a".into(), 0.0), ("b".into(), 68.0)];
        let mut shift: ListShift<&str> =
            ListShift::start(&motion, before, vec![], vec![], 600.0, start).unwrap();
        shift.row("a", 0.0, start);
        shift.row("b", 68.0, start);
        assert!(shift.is_still());
    }

    #[test]
    fn an_interrupted_shift_hands_over_offsets_and_ghosts() {
        let motion = Motion::new(true);
        let start = Instant::now();
        let before = vec![("b".into(), 0.0), ("c".into(), 68.0)];
        let mut first =
            ListShift::start(&motion, before, vec![("b", 0.0)], vec![], 600.0, start).unwrap();
        first.row("c", 0.0, start);
        let mid = at(start, 30);
        let offset = first.offset_of("c", mid);
        assert!(offset > 0.0 && offset < 68.0);
        let carried = first.take_ghosts();
        let second = ListShift::start(
            &motion,
            vec![("c".into(), offset)],
            vec![],
            carried,
            600.0,
            mid,
        )
        .unwrap();
        assert_eq!(second.ghosts_in(0.0, 68.0, mid).len(), 1, "b keeps fading");
    }
}
