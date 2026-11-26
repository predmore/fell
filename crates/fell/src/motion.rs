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

/// A colour or focus change (a pane's focus edge), or content that
/// arrives in place from [`ARRIVE_FROM`]: another thread's messages,
/// another folder's rows.
pub const QUICK: Duration = Duration::from_millis(80);
/// Rows moving, a pane sliding in, a scroll gliding to its target.
pub const SLIDE: Duration = Duration::from_millis(120);
/// Give a restored slot one frame of room before drawing its selection.
const RESTORE_LEAD: Duration = Duration::from_millis(16);
/// A pane coming into view: the conversation replacing the list in a
/// narrow window, or the list coming back. The longest move, so the longest.
pub const PANE: Duration = Duration::from_millis(160);
/// Anything fading in or out on its own: an overlay (palette, folder
/// picker, help, which-key), a notice, a row that left the list. Every
/// exit takes this long, on the gentler [`fade_out`] curve, so nothing
/// that leaves reads as a cut.
pub const FADE: Duration = Duration::from_millis(120);

/// Reading mode lays out both measures before moving the blocks below them.
pub const READING_REFLOW: Duration = Duration::from_millis(140);

#[derive(Clone, Copy)]
pub struct ReadingReflow {
    /// Opacity of the split and reading layouts, respectively.
    pub alpha: [f32; 2],
    /// Interpolation between their natural heights.
    pub height: f32,
}

impl ReadingReflow {
    pub fn opacities(from: [f32; 2], reading: bool, fade: Transition, now: Instant) -> [f32; 2] {
        let entering = usize::from(reading);
        let leaving = 1 - entering;
        let fade_out = Transition::new(fade.started, Duration::from_millis(60));
        let fade_in = Transition::new(
            fade.started + Duration::from_millis(40),
            Duration::from_millis(100),
        );
        let mut alpha = from;
        alpha[leaving] *= fade_out.remaining(now);
        alpha[entering] = lerp(from[entering], 1.0, fade_in.progress(now));
        alpha
    }
}

/// How far an overlay rises while it fades in, in pixels.
pub const RISE_PX: f32 = 6.0;
/// How far a notice rises as it comes in, and again as it leaves, in
/// pixels: a notice replacing another reads as a swap, not a smear.
pub const NOTICE_RISE_PX: f32 = 3.0;
/// How far a pane that comes into view slides in from its side, in pixels.
pub const PANE_SLIDE_PX: f32 = 24.0;
/// Another thread's messages, and another folder's rows, arrive at this
/// opacity and settle to full over [`QUICK`], so switching softens instead
/// of blinking through empty.
pub const ARRIVE_FROM: f32 = 0.4;
/// A glide never starts further than this many viewports from its target:
/// `gg` from far down the list covers the last stretch, not every row.
pub const GLIDE_MAX_VIEWPORTS: f32 = 1.0;

/// Ease-out cubic: fast start, gentle stop.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// How much of something leaving still shows at linear time `t`: a
/// quadratic ease-out, gentler at the start than [`ease_out`], so an exit
/// never reads as a cut. Over [`FADE`] at 60 Hz: 74 %, 52 %, 34 % after
/// one, two and three frames.
pub fn fade_out(t: f32) -> f32 {
    (1.0 - t.clamp(0.0, 1.0)).powi(2)
}

/// Ease-out quadratic: the curve of a long glide ([`Glide::gentle_beyond`]).
/// It leads the clock less than [`ease_out`] early on, so one late frame
/// (up to [`MAX_STEP`] of a [`SLIDE`]) covers 48 % of the way, not 63 %.
pub fn ease_out_quad(t: f32) -> f32 {
    1.0 - fade_out(t)
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

/// The longest step one frame moves the motion [`Clock`]: two frames at 60 Hz.
pub const MAX_STEP: Duration = Duration::from_millis(34);

/// The time transitions run on. It follows the wall clock, except that one
/// frame never moves it more than [`MAX_STEP`]: a frame that comes late
/// (the compositor held it back, the machine is busy) shows the next step
/// of what was moving instead of its end, so nothing jumps. A transition
/// still finishes in its time on any steady frame rate down to 30 Hz.
/// When nothing runs, it catches up with the wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct Clock {
    /// How far motion time is behind the wall clock.
    lag: Duration,
    /// When the last frame that drew motion started (wall clock).
    last: Option<Instant>,
}

impl Clock {
    /// Motion time at wall-clock `real`.
    pub fn at(&self, real: Instant) -> Instant {
        real.checked_sub(self.lag).unwrap_or(real)
    }

    /// Motion time now, for a transition a key or a store reply starts.
    /// The next frame measures its step from here.
    pub fn now(&mut self) -> Instant {
        self.now_at(Instant::now())
    }

    /// [`Self::now`] at wall-clock `real`. Between frames this is the
    /// motion time the next frame draws at: once wall time is more than
    /// [`MAX_STEP`] past the last frame (a stall, a hidden window), that
    /// frame is capped there, so a transition started now must start
    /// there too, or it would sit at its start state until the capped
    /// clock caught up with it.
    pub fn now_at(&mut self, real: Instant) -> Instant {
        match self.last {
            Some(last) => self.at(real.min(last + MAX_STEP)),
            None => {
                self.last = Some(real);
                self.at(real)
            }
        }
    }

    /// A frame starts at wall-clock `real`: the motion time it draws at.
    pub fn frame(&mut self, real: Instant) -> Instant {
        if let Some(last) = self.last {
            let step = real.saturating_duration_since(last);
            if step > MAX_STEP {
                self.lag += step - MAX_STEP;
            }
        }
        self.last = Some(real);
        self.at(real)
    }

    /// Nothing is moving: catch up with the wall clock.
    pub fn idle(&mut self) {
        *self = Clock::default();
    }
}

/// One running transition. Its progress is time-based on the motion
/// [`Clock`], so a slow frame skips ahead (up to [`MAX_STEP`]) instead of
/// stretching the animation.
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

    /// For a fade-out: how much of what is leaving still shows ([`fade_out`]).
    pub fn remaining(&self, now: Instant) -> f32 {
        fade_out(self.linear(now))
    }
}

/// Replace two layers in sequence so their text never paints over itself.
#[derive(Clone, Copy)]
pub struct LayerSwap {
    outgoing: Transition,
    incoming: Transition,
}

pub enum SwapFrame {
    Leaving(f32),
    Arriving(f32),
}

impl LayerSwap {
    pub fn start(motion: Motion, now: Instant, out: Duration, arrival: Duration) -> Option<Self> {
        Some(Self {
            outgoing: motion.start(out, now)?,
            incoming: Transition::new(now + out, arrival),
        })
    }

    pub fn set_arrival_duration(&mut self, duration: Duration) {
        self.incoming.duration = duration;
    }

    pub fn frame(self, now: Instant) -> Option<SwapFrame> {
        if self.outgoing.running(now) {
            Some(SwapFrame::Leaving(self.outgoing.remaining(now)))
        } else if self.incoming.running(now) {
            Some(SwapFrame::Arriving(self.incoming.progress(now)))
        } else {
            None
        }
    }
}

/// Eased progress of an optional transition: 1 (the end state) when there is none.
pub fn progress(transition: Option<&Transition>, now: Instant) -> f32 {
    transition.map_or(1.0, |transition| transition.progress(now))
}

/// A numeric label starts one crossfade per burst. Updates keep replacing
/// its incoming value, so a steady stream never hides the latest count.
pub struct NumberFade<T> {
    outgoing: Option<(T, Transition)>,
    last_change: Option<Instant>,
}

impl<T> Default for NumberFade<T> {
    fn default() -> Self {
        Self {
            outgoing: None,
            last_change: None,
        }
    }
}

impl<T> NumberFade<T> {
    pub fn change(&mut self, old: T, motion: Motion, now: Instant) {
        let quiet = self
            .last_change
            .replace(now)
            .is_none_or(|last| now.saturating_duration_since(last) >= FADE);
        if quiet {
            self.outgoing = motion.start(FADE, now).map(|fade| (old, fade));
        }
    }

    pub fn frame(&self, now: Instant) -> Option<(&T, f32)> {
        let (old, fade) = self.outgoing.as_ref()?;
        fade.running(now).then(|| (old, fade.progress(now)))
    }
}

/// Whether animations are enabled for the current preferences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Motion {
    enabled: bool,
}

impl Motion {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn enabled(self) -> bool {
        self.enabled
    }

    pub fn from_env() -> Self {
        Self::for_preference(crate::settings::appearance().reduce_motion)
    }

    /// The old override is still the default when settings.toml has no value.
    pub fn environment_preference() -> Option<bool> {
        env::var("FELL_REDUCE_MOTION")
            .ok()
            .map(|value| is_true(&value))
    }

    pub fn for_preference(reduce: Option<bool>) -> Self {
        Self::new(!reduce.unwrap_or_else(|| desktop_reduces_motion(gtk_settings_paths())))
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
    /// On the gentler [`ease_out_quad`] curve: a long glide.
    gentle: bool,
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
            gentle: false,
        })
    }

    /// A glide over more than `distance` pixels eases out on the
    /// quadratic curve, so one late frame cannot carry it half the way.
    pub fn gentle_beyond(mut self, distance: f32) -> Glide {
        self.gentle = (self.to - self.from).abs() > distance;
        self
    }

    pub fn value(&self, now: Instant) -> f32 {
        let t = self.transition.linear(now);
        let p = if self.gentle {
            ease_out_quad(t)
        } else {
            ease_out(t)
        };
        lerp(self.from, self.to, p)
    }

    pub fn running(&self, now: Instant) -> bool {
        self.transition.running(now)
    }

    /// Keep the destination in viewport coordinates after its origin moves.
    pub fn move_origin(&mut self, now: Instant, drawn: f32, delta: f32) -> f32 {
        let end = self.transition.started + self.transition.duration;
        self.from = drawn - delta;
        self.transition = Transition::new(now, end.saturating_duration_since(now));
        self.from
    }
}

/// How one row of a list is drawn while the list changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowMotion {
    /// Draw the row this far below its slot (negative: above).
    pub dy: f32,
    pub opacity: f32,
    /// How much of a restored row's height shows, from its top. The slot
    /// opens with the gap the rows below leave. Other rows show whole.
    pub reveal: Option<f32>,
}

impl RowMotion {
    pub const STILL: RowMotion = RowMotion {
        dy: 0.0,
        opacity: 1.0,
        reveal: None,
    };

    pub fn visible_height(self, height: f32, scale: f32) -> f32 {
        self.reveal
            .map_or(height, |share| snap(height * share, scale))
    }
}

/// A row that left the list, drawn where it was while it fades out and the
/// rows around it close the gap.
#[derive(Clone, Debug)]
pub struct Ghost<R> {
    pub row: R,
    /// Its top in list content (0 at the first row), so it scrolls with
    /// the rows around it.
    pub top: f32,
    transition: Transition,
}

impl<R> Ghost<R> {
    pub fn opacity(&self, now: Instant) -> f32 {
        self.transition.remaining(now)
    }

    pub fn running(&self, now: Instant) -> bool {
        self.transition.running(now)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ListChange {
    Shift,
    Replace,
    BatchLeave { row_height: f32 },
}

/// Where the rows of a fixed-height list were before a change, and how
/// they travel to where they are now (FLIP: first, last, invert, play).
///
/// The list itself changes at once. Each row is drawn offset by where it
/// used to be, and the offset eases to zero: rows below an archived row
/// slide up into its place, rows below an undone row slide down to make
/// room and it opens in the gap, and a row that was not on screen arrives
/// in place from [`ARRIVE_FROM`]. Rows that left are [`Ghost`]s. When no
/// row on screen survives the change, or the list is another folder's,
/// every row arrives in place and nothing slides.
#[derive(Clone, Debug)]
pub struct ListShift<R> {
    transition: Transition,
    opening_top: bool,
    /// Rows arriving in place, from [`ARRIVE_FROM`]: never from empty.
    /// Its clock starts on the first frame that draws them, so a late
    /// first frame cannot eat the fade. `None` until then.
    arrive: Option<Transition>,
    /// Row id → its top before the change, in viewport pixels.
    before: HashMap<String, f32>,
    /// Row id → how far it is drawn from its slot when the shift starts,
    /// fixed the first frame the row is drawn. `None`: the row is new.
    resolved: HashMap<String, Option<f32>>,
    ghosts: Vec<Ghost<R>>,
    replaced: bool,
    batch_leave: bool,
    /// A uniform viewport shift can keep the whole list moving together
    /// even if its original travel exceeds the batch limit.
    batch_scroll: Option<f32>,
    /// Furthest a row slides; one further away fades in instead.
    reach: f32,
}

impl<R: Clone> ListShift<R> {
    /// `before`: id and viewport top of each row drawn before the change
    /// (and a few beyond the edges, so rows sliding in from just outside
    /// slide instead of fading). `left`: rows among them that are gone,
    /// with their top in list content as of now. `carried`: ghosts of an
    /// earlier shift still fading, kept so a quick second change does not
    /// cut them off. A replacement fades in the new list. A batch leave
    /// keeps its ghosts even when every tracked row leaves and only slides
    /// rows within three row heights of their slots, unless the viewport
    /// can share one capped offset.
    pub fn start(
        motion: &Motion,
        before: Vec<(String, f32)>,
        left: Vec<(R, f32)>,
        carried: Vec<Ghost<R>>,
        reach: f32,
        change: ListChange,
        now: Instant,
    ) -> Option<ListShift<R>> {
        let transition = motion.start(SLIDE, now)?;
        let batch_leave = matches!(change, ListChange::BatchLeave { .. });
        let replaced = matches!(change, ListChange::Replace)
            || (!batch_leave && !before.is_empty() && left.len() == before.len());
        let reach = match change {
            ListChange::BatchLeave { row_height } => 3.0 * row_height,
            _ => reach,
        };
        let fade = motion.start(FADE, now)?;
        let mut ghosts: Vec<Ghost<R>> = Vec::new();
        if !replaced {
            ghosts.extend(carried.into_iter().filter(|ghost| ghost.running(now)));
            ghosts.extend(left.into_iter().map(|(row, top)| Ghost {
                row,
                top,
                transition: fade,
            }));
        }
        Some(ListShift {
            transition,
            opening_top: false,
            arrive: None,
            before: before.into_iter().collect(),
            resolved: HashMap::new(),
            ghosts,
            replaced,
            batch_leave,
            batch_scroll: None,
            reach,
        })
    }

    /// The viewport scrolled as the batch left. If the visible survivors
    /// all moved together, give every row the same capped starting offset,
    /// including rows entering the clip from beyond the old snapshot.
    pub fn with_batch_scroll(mut self, mut offsets: impl Iterator<Item = f32>) -> Self {
        if self.batch_leave
            && let Some(first) = offsets.next()
            && first.abs() > self.reach
            && offsets.all(|offset| (offset - first).abs() < 0.5)
        {
            self.batch_scroll = Some(first.clamp(-self.reach, self.reach));
        }
        self
    }

    /// A leading arrival opens its slot from the first frame that paints it.
    pub fn top_arrival(mut self) -> Self {
        self.opening_top = true;
        self
    }

    /// The restored row already owns the cursor. Give its slot room on
    /// the first frame even when the motion clock draws at the start time.
    pub fn show_restored_selection(mut self) -> Self {
        if let Some(start) = self.transition.started.checked_sub(RESTORE_LEAD) {
            self.transition.started = start;
        }
        self
    }

    /// The list was replaced (another folder): the old list's rows below
    /// the new list's last row, with their top in list content, have no
    /// row arriving over them, so they fade out where they were over
    /// [`FADE`] instead of vanishing in one frame. A shift that is not a
    /// replacement already fades the rows that left; this does nothing.
    pub fn fading_below(mut self, rows: Vec<(R, f32)>) -> ListShift<R> {
        if self.replaced {
            let fade = Transition::new(self.transition.started, FADE);
            self.ghosts.extend(rows.into_iter().map(|(row, top)| Ghost {
                row,
                top,
                transition: fade,
            }));
        }
        self
    }

    /// How far row `id`, whose slot starts at viewport `top`, starts from
    /// its slot: `None` when it appears where it is instead.
    fn start_for(&self, id: &str, top: f32) -> Option<f32> {
        if self.replaced {
            return None;
        }
        if let Some(offset) = self.batch_scroll {
            return Some(offset);
        }
        match self.before.get(id) {
            Some(old) if (old - top).abs() <= self.reach => Some(old - top),
            _ => None,
        }
    }

    /// Fix row `id`'s start the first time it is asked for.
    fn resolve(&mut self, id: &str, top: f32) -> Option<f32> {
        if let Some(start) = self.resolved.get(id) {
            return *start;
        }
        let start = self.start_for(id, top);
        self.resolved.insert(id.to_string(), start);
        start
    }

    /// How to draw row `id`, whose slot now starts at `top` (viewport
    /// pixels). `below` holds the following rows' ids and slot tops in
    /// order, so restored rows appear as the list makes room for them.
    pub fn row<'a>(
        &mut self,
        id: &str,
        top: f32,
        height: f32,
        below: impl IntoIterator<Item = (&'a str, f32)>,
        now: Instant,
    ) -> RowMotion {
        if std::mem::take(&mut self.opening_top) {
            self.transition = Transition::new(now, Duration::from_millis(160));
        }
        let p = self.transition.progress(now);
        match self.resolve(id, top) {
            Some(dy) => RowMotion {
                dy: dy * (1.0 - p),
                opacity: 1.0,
                reveal: None,
            },
            None => {
                // Consecutive restored rows share the gap opened by the
                // first surviving row below them. Reveal each slot only
                // as that row passes it.
                let reach = self.reach + height;
                let opening = (!self.replaced && !self.batch_leave)
                    .then(|| {
                        below
                            .into_iter()
                            .take_while(|(_, next_top)| *next_top - top <= reach)
                            .find_map(|(next, next_top)| {
                                self.resolve(next, next_top).map(|dy| (next_top, dy))
                            })
                    })
                    .flatten();
                if let Some((next_top, dy)) = opening.filter(|(_, dy)| *dy < -0.5) {
                    let gap = next_top - top + dy * (1.0 - p);
                    RowMotion {
                        dy: 0.0,
                        opacity: p,
                        reveal: Some((gap / height).clamp(0.0, 1.0)),
                    }
                } else {
                    // Arriving in a slot nothing slides into (another
                    // folder, a `G` landing): softened, never blinked
                    // through empty.
                    let duration = if self.replaced || self.batch_leave {
                        FADE
                    } else {
                        QUICK
                    };
                    let arrive = *self
                        .arrive
                        .get_or_insert_with(|| Transition::new(now, duration));
                    RowMotion {
                        dy: 0.0,
                        opacity: lerp(ARRIVE_FROM, 1.0, arrive.progress(now)),
                        reveal: None,
                    }
                }
            }
        }
    }

    /// How far row `id`, whose slot starts at viewport `top`, is drawn
    /// from it right now: an interrupted shift hands this to the next one,
    /// so a row in flight keeps its place. A row not drawn yet (two changes
    /// before one frame) is where it would have been drawn.
    pub fn offset_of(&self, id: &str, top: f32, now: Instant) -> f32 {
        let p = if self.opening_top {
            0.0
        } else {
            self.transition.progress(now)
        };
        let start = match self.resolved.get(id) {
            Some(start) => *start,
            None => self.start_for(id, top),
        };
        start.map_or(0.0, |dy| dy * (1.0 - p))
    }

    /// Furthest outside the viewport a slot can be while its row is visible.
    pub fn overscan(&self, row_height: f32) -> usize {
        if self.replaced {
            return 0;
        }
        (self.reach / row_height).ceil() as usize
    }

    /// A virtualized list skips slots outside the viewport. Keep a moving
    /// row from one of those slots until it has slid fully out of the clip.
    pub fn visible_overflow(
        &self,
        id: &str,
        slot: f32,
        row_height: f32,
        viewport: f32,
        now: Instant,
    ) -> Option<f32> {
        let painted = slot + self.offset_of(id, slot, now);
        (painted < viewport && painted + row_height > 0.0).then_some(painted)
    }

    pub fn has_batch_ghosts(&self) -> bool {
        !self.replaced && (self.ghosts.len() > 1 || (self.batch_leave && !self.ghosts.is_empty()))
    }

    pub fn batch_arrivals(&self) -> bool {
        self.batch_leave
    }

    pub fn ghosts(&self) -> &[Ghost<R>] {
        &self.ghosts
    }

    /// Ghost tops relative to this slot, in list content pixels. A single
    /// ghost keeps its usual slot. For a batch, the first painted slot owns
    /// every ghost so they paint before any live row, including rows that
    /// are sliding across slot boundaries.
    pub fn ghosts_for_slot(
        &self,
        top: f32,
        height: f32,
        first: bool,
        now: Instant,
    ) -> Vec<(R, f32, f32)> {
        let batch = self.has_batch_ghosts();
        if batch && !first {
            return Vec::new();
        }
        self.ghosts
            .iter()
            .filter(|ghost| {
                ghost.running(now) && (batch || (ghost.top >= top && ghost.top < top + height))
            })
            .map(|ghost| (ghost.row.clone(), ghost.top - top, ghost.opacity(now)))
            .collect()
    }

    /// Rows were put above the ghosts' rows (`dy` pixels of them): the
    /// ghosts move down with the rows they were between.
    pub fn move_ghosts(&mut self, dy: f32) {
        for ghost in &mut self.ghosts {
            ghost.top += dy;
        }
    }

    pub fn take_ghosts(&mut self) -> Vec<Ghost<R>> {
        std::mem::take(&mut self.ghosts)
    }

    pub fn running(&self, now: Instant) -> bool {
        self.opening_top
            || self.transition.running(now)
            || self.arrive.is_some_and(|arrive| arrive.running(now))
            || self.ghosts.iter().any(|ghost| ghost.running(now))
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

    #[test]
    fn first_row_arrival_opens_the_gap_from_the_first_painted_frame() {
        let now = Instant::now();
        let first_frame = now + Duration::from_millis(250);
        let mut shift = ListShift::<u8>::start(
            &Motion::new(true),
            vec![("selection".into(), 0.0)],
            Vec::new(),
            Vec::new(),
            640.0,
            ListChange::Shift,
            now,
        )
        .unwrap()
        .top_arrival();
        assert!(shift.running(first_frame));
        let draw = |shift: &mut ListShift<_>, now| {
            shift.row("arrival", 0.0, 68.0, [("selection", 68.0)], now)
        };
        let start = draw(&mut shift, first_frame);
        assert_eq!(start.reveal, Some(0.0));
        assert_eq!(start.opacity, 0.0);
        assert_eq!(shift.offset_of("selection", 68.0, first_frame), -68.0);
        let midpoint = first_frame + Duration::from_millis(80);
        let row = draw(&mut shift, midpoint);
        assert!((0.0..1.0).contains(&row.reveal.unwrap()));
        assert!((0.0..1.0).contains(&row.opacity));
        let end = first_frame + Duration::from_millis(160);
        assert_eq!(draw(&mut shift, end).reveal, Some(1.0));
        assert_eq!(shift.offset_of("selection", 68.0, end), 0.0);
    }

    #[test]
    fn a_layer_swap_starts_the_arrival_after_the_exit() {
        let now = Instant::now();
        let swap = LayerSwap::start(Motion::new(true), now, QUICK, FADE).unwrap();
        assert!(matches!(swap.frame(now), Some(SwapFrame::Leaving(1.0))));
        assert!(matches!(
            swap.frame(now + QUICK - Duration::from_millis(1)),
            Some(SwapFrame::Leaving(opacity)) if opacity < 0.01
        ));
        assert!(matches!(
            swap.frame(now + QUICK),
            Some(SwapFrame::Arriving(0.0))
        ));
        assert!(swap.frame(now + QUICK + FADE).is_none());
        assert!(LayerSwap::start(Motion::new(false), now, QUICK, FADE).is_none());
    }

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn durations_stay_in_the_quick_range() {
        for duration in [QUICK, SLIDE, PANE, FADE] {
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
    fn an_exit_is_gentle_on_its_first_frames() {
        let start = Instant::now();
        let fade = Transition::new(start, FADE);
        assert_eq!(fade.remaining(start), 1.0);
        let frame = Duration::from_micros(16_667);
        assert!(fade.remaining(start + frame) > 0.7);
        assert!(fade.remaining(start + 2 * frame) > 0.5);
        assert_eq!(fade.remaining(at(start, 120)), 0.0);
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
        let shift: Option<ListShift<u8>> = ListShift::start(
            &off,
            vec![("a".into(), 0.0)],
            vec![],
            vec![],
            600.0,
            ListChange::BatchLeave { row_height: 68.0 },
            now,
        );
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

    fn shift<'a>(
        before: &[(&str, f32)],
        left: &[(&'a str, f32)],
        carried: Vec<Ghost<&'a str>>,
        replace: bool,
        now: Instant,
    ) -> ListShift<&'a str> {
        ListShift::start(
            &Motion::new(true),
            before
                .iter()
                .map(|(id, top)| (id.to_string(), *top))
                .collect(),
            left.to_vec(),
            carried,
            600.0,
            if replace {
                ListChange::Replace
            } else {
                ListChange::Shift
            },
            now,
        )
        .unwrap()
    }

    #[test]
    fn rows_below_an_archived_row_slide_up_over_its_ghost() {
        let start = Instant::now();
        // Rows a, b, c at 0, 68, 136. b is archived: c now sits at 68.
        let mut shift = shift(
            &[("a", 0.0), ("b", 68.0), ("c", 136.0)],
            &[("b", 68.0)],
            vec![],
            false,
            start,
        );
        assert_eq!(shift.row("a", 0.0, 68.0, None, start), RowMotion::STILL);
        let c = shift.row("c", 68.0, 68.0, None, start);
        assert_eq!(
            c,
            RowMotion {
                dy: 68.0,
                opacity: 1.0,
                reveal: None,
            },
            "c starts where it was"
        );
        let ghosts = shift.ghosts_for_slot(68.0, 68.0, false, start);
        assert_eq!(ghosts.len(), 1);
        assert_eq!(ghosts[0].0, "b");
        assert_eq!(ghosts[0].1, 0.0);
        assert!(shift.running(at(start, 60)));
        let mid = shift.row("c", 68.0, 68.0, None, at(start, 60));
        assert!(mid.dy > 0.0 && mid.dy < 68.0);
        assert_eq!(
            shift.row("c", 68.0, 68.0, None, at(start, 120)),
            RowMotion::STILL
        );
        assert!(
            shift
                .ghosts_for_slot(68.0, 68.0, false, at(start, 120))
                .is_empty()
        );
        assert!(!shift.running(at(start, 120)));
    }

    #[test]
    fn batch_ghosts_paint_before_every_live_slot_even_when_scrolled() {
        let start = Instant::now();
        let shift = shift(
            &[
                ("a", -18.0),
                ("b", 50.0),
                ("c", 118.0),
                ("d", 186.0),
                ("e", 254.0),
            ],
            &[("a", 50.0), ("b", 118.0), ("d", 254.0)],
            vec![],
            false,
            start,
        );
        // The first live slot starts at content y=68. Ghost a still
        // intersects the viewport above that slot, and d is two slots below.
        assert_eq!(
            shift.ghosts_for_slot(68.0, 68.0, true, start),
            vec![("a", -18.0, 1.0), ("b", 50.0, 1.0), ("d", 186.0, 1.0)]
        );
        assert!(shift.ghosts_for_slot(136.0, 68.0, false, start).is_empty());
        assert!(
            shift
                .ghosts_for_slot(204.0, f32::INFINITY, false, start)
                .is_empty()
        );
    }

    #[test]
    fn a_new_row_fades_in_and_a_far_row_does_not_fly() {
        let start = Instant::now();
        let mut shift = ListShift::<&str>::start(
            &Motion::new(true),
            vec![("a".into(), 0.0), ("far".into(), 0.0)],
            vec![],
            vec![],
            300.0,
            ListChange::Shift,
            start,
        )
        .unwrap();
        assert_eq!(
            shift.row("new", 0.0, 68.0, None, start).opacity,
            ARRIVE_FROM
        );
        assert_eq!(
            shift.row("far", 900.0, 68.0, None, start),
            RowMotion {
                dy: 0.0,
                opacity: ARRIVE_FROM,
                reveal: None,
            }
        );
        assert_eq!(
            shift.row("new", 0.0, 68.0, None, at(start, 80)).opacity,
            1.0
        );
    }

    #[test]
    fn batch_leaves_slide_up_to_three_rows_and_fade_farther_arrivals() {
        let start = Instant::now();
        let h = 68.0;
        let mut shift = ListShift::start(
            &Motion::new(true),
            vec![
                ("far".into(), 10.0 * h),
                ("near".into(), 4.0 * h),
                ("edge".into(), -h),
            ],
            vec![("left one", 0.0), ("left two", h)],
            vec![],
            800.0,
            ListChange::BatchLeave { row_height: h },
            start,
        )
        .unwrap();
        assert_eq!(shift.overscan(h), 3);
        assert_eq!(
            shift.row("far", 0.0, h, [("edge", 2.0 * h)], start),
            RowMotion {
                dy: 0.0,
                opacity: ARRIVE_FROM,
                reveal: None,
            },
            "a far survivor fades even when a moving row crosses its slot"
        );
        assert_eq!(
            shift.row("unseen", 3.0 * h, h, None, start).opacity,
            ARRIVE_FROM
        );
        for (id, top, dy) in [("near", h, 3.0 * h), ("edge", 2.0 * h, -3.0 * h)] {
            assert_eq!(shift.row(id, top, h, None, start).dy, dy);
            for ms in [0, 16, 60, 120] {
                let frame = shift.row(id, top, h, None, at(start, ms));
                assert!(frame.dy.abs() <= 3.0 * h);
                assert_eq!(frame.opacity, 1.0);
            }
        }
        let mid = at(start, 60);
        assert!((ARRIVE_FROM..1.0).contains(&shift.row("far", 0.0, h, None, mid).opacity));
        assert!((0.0..1.0).contains(&shift.ghosts()[0].opacity(mid)));
        let end = start + FADE;
        assert_eq!(shift.row("far", 0.0, h, None, end), RowMotion::STILL);
        assert!(!shift.running(end));
    }

    #[test]
    fn a_uniform_batch_scroll_slides_together_at_the_travel_limit() {
        let start = Instant::now();
        let h = 68.0;
        for direction in [-1.0, 1.0] {
            let offset = direction * 4.0 * h;
            let shift = ListShift::start(
                &Motion::new(true),
                vec![("a".into(), offset), ("b".into(), h + offset)],
                vec![("left", 4.0 * h)],
                vec![],
                800.0,
                ListChange::BatchLeave { row_height: h },
                start,
            )
            .unwrap();
            let mut unequal = shift.clone().with_batch_scroll([offset, 0.0].into_iter());
            assert_eq!(unequal.row("a", 0.0, h, None, start).opacity, ARRIVE_FROM);

            let mut shift = shift.with_batch_scroll([offset, offset].into_iter());
            assert_eq!(shift.offset_of("a", 0.0, start), direction * 3.0 * h);
            for ms in 0..=120 {
                let now = at(start, ms);
                let a = shift.row("a", 0.0, h, None, now);
                let b = shift.row("b", h, h, None, now);
                let entering = shift.row("unseen", 2.0 * h, h, None, now);
                assert_eq!(a, b);
                assert_eq!(a, entering);
                assert_eq!(a.opacity, 1.0);
                let expected = direction * 3.0 * h * (1.0 - ease_out(ms as f32 / 120.0));
                assert!((a.dy - expected).abs() < 0.001);
            }
            assert_eq!(
                shift.row("a", 0.0, h, None, start + SLIDE),
                RowMotion::STILL
            );
        }
    }

    #[test]
    fn batch_leaves_keep_their_ghosts_when_no_tracked_row_survives() {
        let start = Instant::now();
        let mut shift = ListShift::start(
            &Motion::new(true),
            vec![("a".into(), 0.0), ("b".into(), 68.0)],
            vec![("a", 0.0), ("b", 68.0)],
            vec![],
            136.0,
            ListChange::BatchLeave { row_height: 68.0 },
            start,
        )
        .unwrap();
        assert_eq!(
            shift.row("new", 0.0, 68.0, None, start).opacity,
            ARRIVE_FROM
        );
        assert_eq!(shift.ghosts_for_slot(0.0, 68.0, true, start).len(), 2);
    }

    #[test]
    fn a_row_put_back_opens_with_its_gap() {
        let start = Instant::now();
        // Undo puts b back between a and c: c was at 68 and now sits at 136.
        let mut shift = shift(&[("a", 0.0), ("c", 68.0)], &[], vec![], false, start);
        let b = shift.row("b", 68.0, 68.0, Some(("c", 136.0)), start);
        assert_eq!(b.reveal, Some(0.0), "closed while c still covers its slot");
        assert_eq!(b.opacity, 0.0, "it grows with its gap");
        let mid = at(start, 40);
        let b = shift.row("b", 68.0, 68.0, Some(("c", 136.0)), mid);
        let c = shift.row("c", 136.0, 68.0, None, mid);
        // The part of b that shows ends where c is drawn.
        assert!((68.0 + 68.0 * b.reveal.unwrap() - (136.0 + c.dy)).abs() < 0.01);
        assert_eq!(
            shift
                .row("b", 68.0, 68.0, Some(("c", 136.0)), at(start, 120))
                .reveal,
            Some(1.0)
        );
        // A row added at the end has nothing to make room for.
        assert_eq!(shift.row("d", 204.0, 68.0, None, start).reveal, None);
    }

    #[test]
    fn consecutive_restored_rows_reveal_only_the_space_the_list_opens() {
        let start = Instant::now();
        let slots = [("b", 68.0), ("c", 136.0), ("d", 204.0), ("e", 272.0)];
        let mut shift = shift(&[("a", 0.0), ("e", 68.0)], &[], vec![], false, start);

        for ms in [0, 8, 16, 40, 80, 120] {
            let now = at(start, ms);
            let mut revealed = 0.0;
            for (index, (id, top)) in slots[..3].iter().copied().enumerate() {
                let row = shift.row(id, top, 68.0, slots[index + 1..].iter().copied(), now);
                revealed += row.reveal.unwrap() * 68.0;
                assert_eq!(row.dy, 0.0);
            }
            let next = shift.row("e", 272.0, 68.0, None, now);
            assert!((revealed - (272.0 + next.dy - 68.0)).abs() < 0.01);
        }
    }

    #[test]
    fn rows_pushed_below_the_viewport_follow_the_restore() {
        let start = Instant::now();
        let shift = shift(
            &[("a", 0.0), ("b", 68.0), ("c", 136.0)],
            &[],
            vec![],
            false,
            start,
        );
        let viewport = 204.0;
        // Two restored rows put b and c just below the viewport.
        let painted =
            |id, slot, ms| shift.visible_overflow(id, slot, 68.0, viewport, at(start, ms));
        assert_eq!(painted("b", 204.0, 0), Some(68.0));
        assert_eq!(painted("c", 272.0, 0), Some(136.0));
        for ms in [8, 16, 40, 80] {
            let b = painted("b", 204.0, ms).unwrap();
            assert!(b > 68.0 && b < viewport);
            if let Some(c) = painted("c", 272.0, ms) {
                assert!((c - b - 68.0).abs() < 0.01, "{ms} ms");
            }
        }
        assert_eq!(painted("b", 204.0, 120), None);
        assert_eq!(painted("c", 272.0, 120), None);
    }

    #[test]
    fn a_replaced_list_fades_in_without_ghosts() {
        let start = Instant::now();
        let mut replaced = shift(&[("a", 0.0)], &[("a", 0.0)], vec![], false, start);
        assert!(replaced.ghosts_for_slot(0.0, 68.0, false, start).is_empty());
        assert_eq!(
            replaced.row("x", 0.0, 68.0, None, start).opacity,
            ARRIVE_FROM
        );
    }

    #[test]
    fn a_folder_switch_never_paints_an_empty_list() {
        let start = Instant::now();
        // `gs`: Sent's rows replace the inbox's. Rows below the first are
        // asked for with the row below them, as the view does.
        let mut switched = shift(
            &[("a", 0.0), ("b", 68.0), ("c", 136.0)],
            &[("a", 0.0), ("b", 68.0), ("c", 136.0)],
            vec![],
            true,
            start,
        );
        let rows = [("x", 0.0), ("y", 68.0), ("z", 136.0)];
        let mut last = [0.0f32; 3];
        for ms in [0, 1, 16, 33, 50, 66, 80, 120] {
            let now = at(start, ms);
            for (i, (id, top)) in rows.iter().enumerate() {
                let below = rows.get(i + 1).map(|(id, top)| (*id, *top));
                let drawn = switched.row(id, *top, 68.0, below, now);
                assert!(drawn.opacity >= ARRIVE_FROM, "{id} at {ms} ms: {drawn:?}");
                assert!(drawn.opacity >= last[i], "never dims again");
                assert_eq!((drawn.dy, drawn.reveal), (0.0, None));
                last[i] = drawn.opacity;
            }
        }
        assert_eq!(last, [1.0; 3], "full after FADE");
    }

    #[test]
    fn a_stalled_frame_moves_motion_on_by_one_step_at_most() {
        let start = Instant::now();
        let mut clock = Clock::default();
        assert_eq!(clock.frame(start), start);
        let fade = Transition::new(at(start, 16), QUICK);
        // Steady 60 Hz and 30 Hz frames keep wall-clock time.
        let t = clock.frame(at(start, 16));
        assert_eq!(t, at(start, 16));
        let t = clock.frame(at(start, 49));
        assert_eq!(t, at(start, 49));
        // The compositor holds the next frame back 100 ms: the fade moves
        // one step on, not to its end.
        let t = clock.frame(at(start, 149));
        assert_eq!(t, at(start, 49) + MAX_STEP);
        assert!(fade.running(t), "{}", fade.progress(t));
        // Frames after the stall come at the usual pace and finish it.
        let t = clock.frame(at(start, 165));
        assert_eq!(t, at(start, 49 + 34 + 16));
        assert!(!fade.running(t));
        // Once nothing runs, motion time is wall-clock time again.
        clock.idle();
        assert_eq!(clock.frame(at(start, 400)), at(start, 400));
    }

    #[test]
    fn a_transition_started_after_a_gap_starts_on_the_next_frame() {
        let start = Instant::now();
        let mut clock = Clock::default();
        clock.frame(start);
        clock.frame(at(start, 16));
        // A key 80 ms after the last frame, and its frame right after it.
        // That frame is capped one step on; the fade must start there,
        // not 80 ms of wall time ahead of it.
        let began = clock.now_at(at(start, 96));
        assert_eq!(began, at(start, 16) + MAX_STEP);
        let fade = Transition::new(began, QUICK);
        let first = clock.frame(at(start, 97));
        assert_eq!(first, began, "the first frame draws its start");
        let second = clock.frame(at(start, 113));
        assert!(fade.linear(second) > 0.19, "{}", fade.linear(second));
        assert!(fade.running(second));
        // Steady frames from there finish it in its time from the first.
        let mut t = second;
        for ms in [129, 145, 161, 177] {
            t = clock.frame(at(start, ms));
        }
        assert!(!fade.running(t), "{}", fade.linear(t));
        // Within one step of the last frame, a key starts at wall time.
        let soon = clock.now_at(at(start, 187));
        assert_eq!(soon, clock.at(at(start, 187)));
    }

    #[test]
    fn a_transition_started_in_a_hidden_window_starts_when_it_shows() {
        let start = Instant::now();
        let mut clock = Clock::default();
        clock.frame(start);
        // Hidden: no frame for two seconds, then a store reply starts a fade.
        let began = clock.now_at(at(start, 2000));
        let fade = Transition::new(began, QUICK);
        // Another start before the window shows lands on the same time.
        assert_eq!(clock.now_at(at(start, 2500)), began);
        // Shown at last: its first frame draws the start, the next ones run.
        let first = clock.frame(at(start, 3000));
        assert_eq!(fade.linear(first), 0.0);
        let next = clock.frame(at(start, 3016));
        assert!(fade.running(next) && fade.linear(next) > 0.0);
        let mut t = next;
        for ms in [3032, 3048, 3064, 3080] {
            t = clock.frame(at(start, ms));
        }
        assert!(!fade.running(t), "{}", fade.linear(t));
    }

    #[test]
    fn a_long_glide_eases_on_the_gentler_curve() {
        let motion = Motion::new(true);
        let start = Instant::now();
        let short = Glide::start(&motion, 0.0, 100.0, 0.0, SLIDE, start)
            .unwrap()
            .gentle_beyond(300.0);
        let long = Glide::start(&motion, 0.0, 600.0, 0.0, SLIDE, start)
            .unwrap()
            .gentle_beyond(300.0);
        // One late frame: a whole MAX_STEP of the glide at once.
        let late = start + MAX_STEP;
        assert!(
            short.value(late) / 100.0 > 0.6,
            "short keeps the snappier curve"
        );
        let share = long.value(late) / 600.0;
        assert!(share < 0.5, "one late frame covers {share}");
        assert_eq!(long.value(at(start, 120)), 600.0, "lands exactly");
        assert_eq!(ease_out_quad(0.0), 0.0);
        assert_eq!(ease_out_quad(1.0), 1.0);
    }

    #[test]
    fn a_late_first_frame_does_not_eat_the_arrival() {
        let start = Instant::now();
        let mut switched = shift(&[("a", 0.0)], &[("a", 0.0)], vec![], true, start);
        // The rows landed at `start`, but the first frame that draws them
        // comes 90 ms later, longer than the whole fade.
        let first = at(start, 90);
        assert_eq!(
            switched.row("x", 0.0, 68.0, None, first).opacity,
            ARRIVE_FROM
        );
        assert!(switched.running(first), "the fade has only begun");
        let mid = switched.row("x", 0.0, 68.0, None, at(start, 110)).opacity;
        assert!(mid > ARRIVE_FROM && mid < 1.0, "{mid}");
        // A row drawn later than the first one joins the same fade.
        assert_eq!(
            switched.row("y", 68.0, 68.0, None, at(start, 110)).opacity,
            mid
        );
        assert_eq!(
            switched.row("x", 0.0, 68.0, None, at(start, 210)).opacity,
            1.0
        );
        assert!(!switched.running(at(start, 210)));
    }

    #[test]
    fn another_folder_drops_ghosts_even_when_a_row_is_shared() {
        let start = Instant::now();
        let first = shift(
            &[("a", 0.0), ("b", 68.0)],
            &[("a", 0.0)],
            vec![],
            false,
            start,
        );
        let carried = first.ghosts.clone();
        // `e` then `gs`: Sent shares b with the inbox, and a is still fading.
        let mut switched = shift(
            &[("b", 0.0), ("c", 68.0)],
            &[("c", 68.0)],
            carried,
            true,
            at(start, 20),
        );
        assert!(
            switched
                .ghosts_for_slot(0.0, 1000.0, false, at(start, 20))
                .is_empty()
        );
        let b = switched.row("b", 0.0, 68.0, None, at(start, 20));
        assert_eq!(b.dy, 0.0, "a shared row fades in with the rest");
        assert!(b.opacity < 1.0);
    }

    #[test]
    fn a_shorter_folder_fades_out_the_rows_below_its_end() {
        let start = Instant::now();
        // `gs` from a long inbox: Sent has two rows, the inbox showed four.
        let mut switched = shift(
            &[("a", 0.0), ("b", 68.0), ("c", 136.0), ("d", 204.0)],
            &[],
            vec![],
            true,
            start,
        )
        .fading_below(vec![("c", 136.0), ("d", 204.0)]);
        // Sent's rows arrive in place; the inbox's last two fade below them.
        let fading = switched.ghosts_for_slot(68.0, f32::INFINITY, false, start);
        assert_eq!(fading.len(), 2);
        assert!(fading.iter().all(|(_, _, opacity)| *opacity == 1.0));
        let row = switched.row("s1", 0.0, 68.0, None, start);
        assert!(row.opacity < 1.0, "Sent's rows arrive");
        let mid = switched.ghosts_for_slot(68.0, f32::INFINITY, false, at(start, 60));
        assert!(
            mid.iter()
                .all(|(_, _, opacity)| *opacity > 0.0 && *opacity < 1.0)
        );
        assert!(switched.running(at(start, 100)));
        assert!(
            switched
                .ghosts_for_slot(68.0, f32::INFINITY, false, at(start, 120))
                .is_empty()
        );
        // Not a replacement: nothing is added (rows that left are ghosts already).
        let kept = shift(&[("a", 0.0)], &[], vec![], false, start).fading_below(vec![("z", 68.0)]);
        assert!(
            kept.ghosts_for_slot(0.0, f32::INFINITY, false, start)
                .is_empty()
        );
    }

    #[test]
    fn a_change_that_moves_nothing_is_still() {
        let start = Instant::now();
        // New mail above the viewport: the list scrolled by the same amount.
        let mut shift = shift(&[("a", 0.0), ("b", 68.0)], &[], vec![], false, start);
        shift.row("a", 0.0, 68.0, None, start);
        shift.row("b", 68.0, 68.0, None, start);
        assert!(shift.is_still());
    }

    #[test]
    fn an_interrupted_shift_hands_over_offsets_and_ghosts() {
        let start = Instant::now();
        let mut first = shift(
            &[("b", 0.0), ("c", 68.0)],
            &[("b", 0.0)],
            vec![],
            false,
            start,
        );
        first.row("c", 0.0, 68.0, None, start);
        let mid = at(start, 30);
        let offset = first.offset_of("c", 0.0, mid);
        assert!(offset > 0.0 && offset < 68.0);
        let carried = first.take_ghosts();
        let second = shift(&[("c", offset)], &[], carried, false, mid);
        assert_eq!(
            second.ghosts_for_slot(0.0, 68.0, false, mid).len(),
            1,
            "b keeps fading"
        );
    }

    #[test]
    fn two_changes_before_a_frame_hand_over_unpainted_offsets() {
        let start = Instant::now();
        // a, b, c, d at 0, 68, 136, 204. `e` on a, then `e` on b before
        // any frame drew the first shift: nothing called `row` yet.
        let first = shift(
            &[("a", 0.0), ("b", 68.0), ("c", 136.0), ("d", 204.0)],
            &[("a", 0.0)],
            vec![],
            false,
            start,
        );
        // After the first archive c's slot is at 68, and it is drawn at 136.
        assert_eq!(first.offset_of("c", 68.0, start), 68.0);
        let later = at(start, 30);
        let c = first.offset_of("c", 68.0, later);
        assert!(c > 0.0 && c < 68.0, "lerped by progress: {c}");
        // The second shift starts from those pixels, not from the slots.
        let mut second = shift(
            &[
                ("b", 0.0 + first.offset_of("b", 0.0, later)),
                ("c", 68.0 + c),
            ],
            &[("b", 0.0)],
            vec![],
            false,
            later,
        );
        let drawn = second.row("c", 0.0, 68.0, None, later);
        assert!((drawn.dy - (68.0 + c)).abs() < 0.01, "no snap: {drawn:?}");
    }

    #[test]
    fn ghosts_live_in_content_and_move_with_rows_put_above() {
        let start = Instant::now();
        // b left from content y 136 (row 2).
        let mut shift = shift(
            &[("a", 68.0), ("b", 136.0)],
            &[("b", 136.0)],
            vec![],
            false,
            start,
        );
        // Scrolling never moves it: slots are asked for in content pixels.
        assert_eq!(shift.ghosts_for_slot(136.0, 68.0, false, start).len(), 1);
        // Two rows arrived above: it is now in row 4's slot.
        shift.move_ghosts(136.0);
        assert!(shift.ghosts_for_slot(136.0, 68.0, false, start).is_empty());
        assert_eq!(shift.ghosts_for_slot(272.0, 68.0, false, start).len(), 1);
    }
}
