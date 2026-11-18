//! The inbox window's running transitions, built on [`crate::motion`].
//!
//! Most of them start from what changed between two frames, not from the
//! code that changed it: [`InboxMotion::observe`] compares the session with
//! what the last frame drew, so every path that opens the palette, shows a
//! notice or switches panes gets the same transition. The list and the
//! scroll glides start where the view changes them, because they need the
//! positions from before the change.

use std::time::Instant;

use crate::keymap::KeyId;
use crate::model::{FolderIntent, Overlay, Session, Toast};
use crate::motion::{self, Ghost, Glide, ListShift, Motion, Transition};
use crate::present::{Pane, ThreadRow};

/// The old selection's highlight starts its fade from this much, so on a
/// slow frame two rows never look selected.
pub const SELECTION_FROM: f32 = 0.6;

/// The status bar's notice and key hints as this frame draws them.
pub struct StatusMotion {
    /// The notice on screen and its opacity.
    pub notice: Option<(Toast, f32)>,
    /// A notice leaving, fading where it was: a key cleared it, or another
    /// notice replaced it.
    pub leaving: Option<(Toast, f32)>,
    /// How much of the key hints shows. A notice takes their room, so they
    /// fade out as it comes and back in as it leaves.
    pub hints: f32,
}

/// What an overlay or which-key showed, kept from the frame before it
/// closed: the key that closed it may already have cleared its query, so
/// it fades out as it was, not as the session is now.
#[derive(Clone, Debug)]
pub struct Layer {
    overlay: Overlay,
    chord: Vec<KeyId>,
    palette_query: String,
    palette_index: usize,
    folder_query: String,
    folder_index: usize,
    folder_intent: FolderIntent,
}

impl Layer {
    fn of(session: &Session) -> Self {
        Self {
            overlay: session.overlay.clone(),
            chord: session.chord.clone(),
            palette_query: session.palette_query.clone(),
            palette_index: session.palette_index,
            folder_query: session.folder_query.clone(),
            folder_index: session.folder_index,
            folder_intent: session.folder_intent,
        }
    }

    /// Trade this layer's state with the session's. Call it once to draw
    /// the layer as it was, and once more to put the session back.
    pub fn swap(&mut self, session: &mut Session) {
        std::mem::swap(&mut self.overlay, &mut session.overlay);
        std::mem::swap(&mut self.chord, &mut session.chord);
        std::mem::swap(&mut self.palette_query, &mut session.palette_query);
        std::mem::swap(&mut self.palette_index, &mut session.palette_index);
        std::mem::swap(&mut self.folder_query, &mut session.folder_query);
        std::mem::swap(&mut self.folder_index, &mut session.folder_index);
        std::mem::swap(&mut self.folder_intent, &mut session.folder_intent);
    }
}

/// A pane that just came into view, sliding in from its side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneEntry {
    pub opacity: f32,
    /// Offset from its place, in pixels (negative: from the left).
    pub dx: f32,
}

pub struct InboxMotion {
    pub motion: Motion,
    /// Rows moving after an archive, undo, sync or folder switch.
    pub list: Option<ListShift<ThreadRow>>,
    /// The list viewport gliding to the selection (`Ctrl-d`, `gg`, `G`).
    pub list_glide: Option<Glide>,
    /// The conversation gliding to a page or a message.
    pub conv_glide: Option<Glide>,
    toast_in: Option<Transition>,
    /// The notice that is leaving and its fade.
    toast_out: Option<(Toast, Transition)>,
    last_toast: Option<Toast>,
    /// The key hints fading out as a notice takes their room, and how
    /// much of them showed when it started.
    hints_out: Option<Transition>,
    hints_from: f32,
    /// A chord was pending last frame: the bar showed its prefix, not the hints.
    last_chord: bool,
    which: Option<Transition>,
    which_out: Option<(Layer, Transition)>,
    last_which: bool,
    overlay: Option<Transition>,
    overlay_out: Option<(Layer, Transition)>,
    last_overlay: Overlay,
    /// The overlay or which-key as last drawn, for its fade-out.
    last_layer: Option<Layer>,
    /// The pane that came into view and the side it comes from.
    pane: Option<(Transition, Pane)>,
    last_panes: (bool, bool),
    last_layout: (Pane, bool),
    /// The conversation's focus edge changing colour.
    focus: Option<Transition>,
    /// The row the selection just left, its highlight fading out.
    selection_out: Option<(String, Transition)>,
    last_selected: Option<String>,
    /// Another thread's messages just arrived in the conversation.
    arrival: Option<Transition>,
    /// Where a glide a key interrupted was drawn, so the next glide starts
    /// there instead of snapping to the old target first.
    list_from: Option<f32>,
    conv_from: Option<f32>,
}

/// Scroll offsets a settle jumped to, for the view to apply.
#[derive(Default)]
pub struct Settled {
    pub list: Option<f32>,
    pub conversation: Option<f32>,
}

impl InboxMotion {
    pub fn new(motion: Motion) -> Self {
        Self {
            motion,
            list: None,
            list_glide: None,
            conv_glide: None,
            toast_in: None,
            toast_out: None,
            last_toast: None,
            hints_out: None,
            hints_from: 1.0,
            last_chord: false,
            which: None,
            which_out: None,
            last_which: false,
            overlay: None,
            overlay_out: None,
            last_overlay: Overlay::None,
            last_layer: None,
            pane: None,
            last_panes: (true, true),
            last_layout: (Pane::List, false),
            focus: None,
            selection_out: None,
            last_selected: None,
            arrival: None,
            list_from: None,
            conv_from: None,
        }
    }

    /// Start the transitions for what changed since the last frame.
    /// `panes`: which panes this frame shows (list, conversation).
    pub fn observe(&mut self, session: &Session, panes: (bool, bool), now: Instant) {
        let start = |duration| self.motion.start(duration, now);
        if session.toast != self.last_toast {
            match (&session.toast, self.last_toast.take()) {
                (Some(toast), old) => {
                    // The same notice again (a second `e` in a burst): it
                    // stays up instead of blinking out and back.
                    let returning = self
                        .toast_out
                        .as_ref()
                        .is_some_and(|(leaving, _)| leaving == toast);
                    if returning {
                        self.toast_in = None;
                        self.toast_out = None;
                    } else {
                        self.toast_in = start(motion::FADE);
                        match old {
                            // Another notice replaced this one: cross-fade.
                            Some(old) => {
                                self.toast_out = start(motion::QUICK).map(|fade| (old, fade));
                            }
                            // The hints make room as the notice comes, from
                            // what shows of them now. A notice still leaving
                            // keeps fading under the new one.
                            // A chord's prefix held the hints' place: the
                            // hints were not showing, so they stay hidden.
                            None => {
                                self.hints_from = match &self.toast_out {
                                    _ if self.last_chord => 0.0,
                                    Some((_, fade)) => fade.progress(now),
                                    None => 1.0,
                                };
                                self.hints_out = start(motion::QUICK);
                            }
                        }
                    }
                }
                (None, Some(old)) => {
                    self.toast_in = None;
                    self.hints_out = None;
                    self.toast_out = start(motion::FADE_OUT).map(|fade| (old, fade));
                }
                (None, None) => {}
            }
            self.last_toast = session.toast.clone();
        }

        let which =
            session.which_visible && session.overlay == Overlay::None && !session.chord.is_empty();
        if which != self.last_which {
            if which {
                self.which = start(motion::FADE);
                self.which_out = None;
            } else {
                self.which = None;
                self.which_out = self
                    .last_layer
                    .clone()
                    .filter(|layer| !layer.chord.is_empty())
                    .and_then(|layer| start(motion::QUICK).map(|fade| (layer, fade)));
            }
        }
        self.last_which = which;
        self.last_chord = !session.chord.is_empty();

        if session.overlay != self.last_overlay {
            if session.overlay == Overlay::None {
                // Gone for input at once; only its pixels fade.
                self.overlay = None;
                self.overlay_out = self
                    .last_layer
                    .clone()
                    .filter(|layer| layer.overlay != Overlay::None)
                    .and_then(|layer| start(motion::QUICK).map(|fade| (layer, fade)));
            } else {
                self.overlay = start(motion::FADE);
                self.overlay_out = None;
            }
            self.last_overlay = session.overlay.clone();
        }
        self.last_layer = (which || session.overlay != Overlay::None).then(|| Layer::of(session));

        // Only a key moves the panes; a window resized across the narrow
        // threshold shows its panes at once.
        let layout = (session.pane, session.full_width);
        if layout != self.last_layout {
            if panes != self.last_panes {
                let entering = match (self.last_panes, panes) {
                    ((_, false), (_, true)) | ((true, true), (false, true)) => {
                        Some(Pane::Conversation)
                    }
                    ((false, _), (true, _)) => Some(Pane::List),
                    _ => None,
                };
                self.pane = entering.and_then(|pane| start(motion::PANE).map(|t| (t, pane)));
            }
            if layout.0 != self.last_layout.0 {
                self.focus = start(motion::QUICK);
            }
            self.last_layout = layout;
        }
        self.last_panes = panes;

        if session.cursor_id != self.last_selected {
            // The new row is highlighted at once; only the old one fades.
            self.selection_out = self
                .last_selected
                .take()
                .and_then(|old| start(motion::QUICK).map(|fade| (old, fade)));
            self.last_selected = session.cursor_id.clone();
        }
    }

    /// Another thread's messages replaced the conversation.
    pub fn thread_arrived(&mut self, now: Instant) {
        self.arrival = self.motion.start(motion::QUICK, now);
    }

    /// Jump every transition to its end state: a key acts on what the
    /// reader will see, never on a frame in between. Glides return their
    /// targets for the view to apply; where they were drawn is kept for a
    /// glide the same key starts ([`Self::glide_from_list`]) and for the
    /// rows a list change animates from ([`Self::painted_list_offset`]).
    /// Rows still sliding keep sliding: the next list change starts from
    /// where they are drawn, and the list the key acts on is already final.
    /// What is leaving (a notice, an overlay) keeps fading: no key acts on
    /// it.
    pub fn settle(&mut self, now: Instant) -> Settled {
        let list = self.list_glide.take();
        let conversation = self.conv_glide.take();
        self.list_from = list.map(|glide| glide.value(now));
        self.conv_from = conversation.map(|glide| glide.value(now));
        self.toast_in = None;
        self.hints_out = None;
        self.which = None;
        self.overlay = None;
        self.pane = None;
        self.focus = None;
        self.selection_out = None;
        self.arrival = None;
        Settled {
            list: list.map(|glide| glide.to),
            conversation: conversation.map(|glide| glide.to),
        }
    }

    /// Where a list glide should start: where the one a key interrupted
    /// was drawn, else `offset`.
    pub fn glide_from_list(&mut self, offset: f32) -> f32 {
        self.list_from.take().unwrap_or(offset)
    }

    /// The list's offset as the last frame painted it, when a key settled
    /// a glide and moved the offset to its target before the next paint.
    pub fn painted_list_offset(&self) -> Option<f32> {
        self.list_from
    }

    pub fn glide_from_conversation(&mut self, offset: f32) -> f32 {
        self.conv_from.take().unwrap_or(offset)
    }

    /// The key is handled: a glide it did not restart stays at its target.
    pub fn end_key(&mut self) {
        self.list_from = None;
        self.conv_from = None;
    }

    /// The wheel took over: stop gliding where the viewport is.
    pub fn cancel_glides(&mut self) {
        self.list_glide = None;
        self.conv_glide = None;
    }

    /// Drop what finished. True while anything still needs frames.
    pub fn tick(&mut self, now: Instant) -> bool {
        fn keep(transition: &mut Option<Transition>, now: Instant) -> bool {
            if transition.is_some_and(|t| !t.running(now)) {
                *transition = None;
            }
            transition.is_some()
        }
        fn keep_with<T>(fade: &mut Option<(T, Transition)>, now: Instant) -> bool {
            if fade.as_ref().is_some_and(|(_, t)| !t.running(now)) {
                *fade = None;
            }
            fade.is_some()
        }
        let mut running = false;
        running |= keep(&mut self.toast_in, now);
        running |= keep_with(&mut self.toast_out, now);
        running |= keep(&mut self.hints_out, now);
        running |= keep(&mut self.which, now);
        running |= keep_with(&mut self.which_out, now);
        running |= keep(&mut self.overlay, now);
        running |= keep_with(&mut self.overlay_out, now);
        running |= keep(&mut self.focus, now);
        running |= keep(&mut self.arrival, now);
        running |= keep_with(&mut self.selection_out, now);
        if self.pane.is_some_and(|(t, _)| !t.running(now)) {
            self.pane = None;
        }
        running |= self.pane.is_some();
        if self
            .list
            .as_ref()
            .is_some_and(|shift| !shift.running(now) || shift.is_still())
        {
            self.list = None;
        }
        running |= self.list.is_some();
        // Glides end in the view, which sets their last offset.
        running |= self.list_glide.is_some() || self.conv_glide.is_some();
        running
    }

    /// Hand the ghosts of a shift still running to the next one.
    pub fn carried_ghosts(&mut self) -> Vec<Ghost<ThreadRow>> {
        self.list
            .as_mut()
            .map(ListShift::take_ghosts)
            .unwrap_or_default()
    }

    /// Rows were put above: rows that left move down with their neighbours.
    pub fn move_ghosts(&mut self, dy: f32) {
        if let Some(shift) = &mut self.list {
            shift.move_ghosts(dy);
        }
    }

    /// Where row `id`, whose slot starts at viewport `top`, is drawn
    /// relative to it right now.
    pub fn row_offset(&self, id: &str, top: f32, now: Instant) -> f32 {
        self.list
            .as_ref()
            .map_or(0.0, |shift| shift.offset_of(id, top, now))
    }

    pub fn status(&self, session: &Session, now: Instant) -> StatusMotion {
        let notice = session
            .toast
            .clone()
            .map(|toast| (toast, motion::progress(self.toast_in.as_ref(), now)));
        let leaving = self
            .toast_out
            .as_ref()
            .map(|(toast, fade)| (toast.clone(), 1.0 - fade.progress(now)));
        let hints = match (&notice, &leaving) {
            (Some(_), _) => {
                self.hints_from * (1.0 - motion::progress(self.hints_out.as_ref(), now))
            }
            (None, Some((_, opacity))) => 1.0 - opacity,
            (None, None) => 1.0,
        };
        StatusMotion {
            notice,
            leaving,
            hints,
        }
    }

    /// Opacity and rise (pixels still to travel up) of the which-key popup.
    pub fn which(&self, now: Instant) -> (f32, f32) {
        let p = motion::progress(self.which.as_ref(), now);
        (p, motion::RISE_PX * (1.0 - p))
    }

    /// Opacity and drop (pixels still to travel down) of an overlay.
    pub fn overlay(&self, now: Instant) -> (f32, f32) {
        let p = motion::progress(self.overlay.as_ref(), now);
        (p, motion::RISE_PX * (1.0 - p))
    }

    /// An overlay that just closed, as it was, and its opacity.
    pub fn closing_overlay(&mut self, now: Instant) -> Option<(&mut Layer, f32)> {
        self.overlay_out
            .as_mut()
            .map(|(layer, fade)| (layer, 1.0 - fade.progress(now)))
    }

    /// The which-key popup that just closed, as it was, and its opacity.
    pub fn closing_which(&mut self, now: Instant) -> Option<(&mut Layer, f32)> {
        self.which_out
            .as_mut()
            .map(|(layer, fade)| (layer, 1.0 - fade.progress(now)))
    }

    pub fn pane_entry(&self, pane: Pane, now: Instant) -> PaneEntry {
        match self.pane {
            Some((transition, entering)) if entering == pane => {
                let p = transition.progress(now);
                let side = if pane == Pane::List { -1.0 } else { 1.0 };
                PaneEntry {
                    opacity: p,
                    dx: side * motion::PANE_SLIDE_PX * (1.0 - p),
                }
            }
            _ => PaneEntry {
                opacity: 1.0,
                dx: 0.0,
            },
        }
    }

    /// How much of the selection highlight row `id` still shows, when the
    /// selection just left it.
    pub fn selection_left(&self, id: &str, now: Instant) -> Option<f32> {
        self.selection_out
            .as_ref()
            .filter(|(old, _)| old == id)
            .map(|(_, fade)| SELECTION_FROM * (1.0 - fade.progress(now)))
    }

    /// Opacity of the conversation's subject and messages: another
    /// thread's arrive from [`motion::ARRIVE_FROM`], so the pane never
    /// blinks to empty.
    pub fn arrival(&self, now: Instant) -> f32 {
        motion::lerp(
            motion::ARRIVE_FROM,
            1.0,
            motion::progress(self.arrival.as_ref(), now),
        )
    }

    /// How far the conversation is towards "focused": its edge and the
    /// message card under the cursor follow this.
    pub fn focus_edge(&self, pane: Pane, now: Instant) -> f32 {
        let p = motion::progress(self.focus.as_ref(), now);
        if pane == Pane::Conversation {
            p
        } else {
            1.0 - p
        }
    }

    /// Progress of a focus change between the panes: the key hints, which
    /// differ per pane, fade in with it.
    pub fn focus_change(&self, now: Instant) -> f32 {
        motion::progress(self.focus.as_ref(), now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use crate::model::SyncKind;
    use std::time::Duration;

    fn session() -> Session {
        Session::new("A", "a@x", SyncKind::LocalOffline, Keymap::default())
    }

    fn later(now: Instant, ms: u64) -> Instant {
        now + Duration::from_millis(ms)
    }

    fn notice_opacity(motion: &InboxMotion, session: &Session, now: Instant) -> Option<f32> {
        motion
            .status(session, now)
            .notice
            .map(|(_, opacity)| opacity)
    }

    #[test]
    fn a_notice_fades_in_then_out_after_a_key() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, true), now);
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), now);
        assert_eq!(notice_opacity(&motion, &session, now), Some(0.0));
        assert!(motion.tick(later(now, 40)));
        let shown = later(now, 200);
        assert!(!motion.tick(shown));
        assert_eq!(notice_opacity(&motion, &session, shown), Some(1.0));
        session.clear_toast();
        motion.observe(&session, (true, true), shown);
        let status = motion.status(&session, shown);
        assert!(status.notice.is_none());
        assert_eq!(status.leaving.as_ref().unwrap().1, 1.0);
        assert_eq!(status.hints, 0.0);
        let mid = motion.status(&session, later(shown, 80));
        assert!(mid.leaving.unwrap().1 < 1.0);
        assert!(
            mid.hints > 0.0 && mid.hints < 1.0,
            "hints come back as it leaves"
        );
        assert!(!motion.tick(later(shown, 200)));
        let gone = motion.status(&session, later(shown, 200));
        assert!(gone.leaving.is_none());
        assert_eq!(gone.hints, 1.0);
    }

    #[test]
    fn the_hints_fade_out_quicker_than_a_notice_fades_in() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, true), now);
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.status(&session, now).hints, 1.0, "no pop");
        let mid = motion.status(&session, later(now, 40));
        assert!(mid.hints > 0.0 && mid.hints < 1.0);
        let quick = motion.status(&session, later(now, 80));
        assert_eq!(quick.hints, 0.0, "gone after QUICK");
        assert!(quick.notice.unwrap().1 < 1.0, "still fading in over FADE");
    }

    #[test]
    fn hints_a_chord_replaced_do_not_come_back_to_fade() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.chord = vec![KeyId::Space];
        motion.observe(&session, (true, true), now);
        // `space s` with no account: the chord ends in a notice.
        session.chord.clear();
        session.show_toast("No account is set up", true);
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.status(&session, now).hints, 0.0);
    }

    #[test]
    fn a_notice_replacing_another_cross_fades() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.show_toast("Undone", false);
        motion.observe(&session, (true, true), now);
        motion.settle(now);
        session.show_toast("Archived · u", false);
        let swap = later(now, 300);
        motion.observe(&session, (true, true), swap);
        let status = motion.status(&session, swap);
        assert_eq!(status.notice.as_ref().unwrap().0.text, "Archived · u");
        assert_eq!(status.notice.unwrap().1, 0.0);
        let (old, opacity) = status.leaving.unwrap();
        assert_eq!(old.text, "Undone");
        assert_eq!(opacity, 1.0);
        assert_eq!(status.hints, 0.0);
        let after = motion.status(&session, later(swap, 80));
        assert!(
            after.leaving.is_none_or(|(_, o)| o == 0.0),
            "old out over QUICK"
        );
        assert!(after.notice.unwrap().1 < 1.0, "new in over FADE");
    }

    #[test]
    fn the_same_notice_again_stays_up() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.show_toast("Archived · u", false);
        motion.observe(&session, (true, true), now);
        motion.settle(now);
        session.clear_toast();
        motion.observe(&session, (true, true), now);
        session.show_toast("Archived · u", false);
        let back = later(now, 10);
        motion.observe(&session, (true, true), back);
        let status = motion.status(&session, back);
        assert_eq!(
            status.notice.unwrap().1,
            1.0,
            "no blink between two archives"
        );
        assert!(status.leaving.is_none());
        assert_eq!(status.hints, 0.0);
    }

    #[test]
    fn a_key_jumps_overlays_and_glides_to_their_end() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Palette;
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.overlay(now).0, 0.0);
        motion.list_glide = Glide::start(&motion.motion, 0.0, -680.0, 0.0, motion::SLIDE, now);
        let settled = motion.settle(now);
        assert_eq!(settled.list, Some(-680.0));
        assert_eq!(motion.overlay(now), (1.0, 0.0));
        assert!(!motion.tick(now));
        // A list change the same key makes starts from the painted rows.
        assert_eq!(motion.painted_list_offset(), Some(0.0));
        // A glide the same key starts begins where the old one was drawn.
        assert_eq!(motion.glide_from_list(-680.0), 0.0);
        assert_eq!(motion.glide_from_list(-680.0), -680.0);
        motion.end_key();
        assert_eq!(motion.painted_list_offset(), None);
    }

    #[test]
    fn a_closed_overlay_fades_out_as_it_was() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Palette;
        session.palette_query = "arch".into();
        motion.observe(&session, (true, true), now);
        // Enter runs the item and closes the palette in one key.
        motion.settle(now);
        session.overlay = Overlay::None;
        session.palette_query.clear();
        motion.observe(&session, (true, true), now);
        let (layer, opacity) = motion.closing_overlay(now).unwrap();
        assert_eq!(opacity, 1.0);
        assert_eq!(layer.overlay, Overlay::Palette);
        layer.swap(&mut session);
        assert_eq!(session.palette_query, "arch", "drawn as it was");
        assert_eq!(session.overlay, Overlay::Palette);
        layer.swap(&mut session);
        assert_eq!(session.overlay, Overlay::None, "the session is put back");
        assert!(session.palette_query.is_empty());
        // A key during the fade does not cut it off, and it ends in QUICK.
        motion.settle(later(now, 40));
        assert!(motion.closing_overlay(later(now, 40)).unwrap().1 < 1.0);
        assert!(!motion.tick(later(now, 80)));
        assert!(motion.closing_overlay(later(now, 80)).is_none());
    }

    #[test]
    fn a_new_overlay_replaces_one_still_fading_out() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Help;
        motion.observe(&session, (true, true), now);
        session.overlay = Overlay::None;
        motion.observe(&session, (true, true), now);
        assert!(motion.closing_overlay(now).is_some());
        session.overlay = Overlay::Palette;
        motion.observe(&session, (true, true), later(now, 20));
        assert!(motion.closing_overlay(later(now, 20)).is_none());
    }

    #[test]
    fn which_key_fades_out_with_its_chord() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.chord = vec![KeyId::Char('g')];
        session.which_visible = true;
        motion.observe(&session, (true, true), now);
        session.chord.clear();
        session.which_visible = false;
        motion.observe(&session, (true, true), now);
        let (layer, opacity) = motion.closing_which(now).unwrap();
        assert_eq!(opacity, 1.0);
        layer.swap(&mut session);
        assert_eq!(session.chord, vec![KeyId::Char('g')]);
        layer.swap(&mut session);
        assert!(session.chord.is_empty());
    }

    #[test]
    fn the_old_selection_fades_while_the_new_one_shows_at_once() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        session.cursor_id = Some("a".into());
        motion.observe(&session, (true, true), now);
        session.cursor_id = Some("b".into());
        motion.observe(&session, (true, true), now);
        assert_eq!(motion.selection_left("a", now), Some(SELECTION_FROM));
        assert_eq!(motion.selection_left("b", now), None);
        assert!(!motion.tick(later(now, 100)));
        assert_eq!(motion.selection_left("a", later(now, 100)), None);
    }

    #[test]
    fn the_conversation_slides_in_when_it_replaces_the_list() {
        let mut motion = InboxMotion::new(Motion::new(true));
        let mut session = session();
        let now = Instant::now();
        motion.observe(&session, (true, false), now);
        session.pane = Pane::Conversation;
        motion.observe(&session, (false, true), now);
        let entry = motion.pane_entry(Pane::Conversation, now);
        assert_eq!(entry.opacity, 0.0);
        assert!(entry.dx > 0.0, "from the right");
        assert_eq!(motion.pane_entry(Pane::List, now).opacity, 1.0);
        assert_eq!(motion.focus_change(now), 0.0, "the hints fade in");
        session.pane = Pane::List;
        let back = later(now, 300);
        motion.observe(&session, (true, false), back);
        assert!(
            motion.pane_entry(Pane::List, back).dx < 0.0,
            "from the left"
        );
    }

    #[test]
    fn reduced_motion_draws_end_states() {
        let mut motion = InboxMotion::new(Motion::new(false));
        let mut session = session();
        let now = Instant::now();
        session.overlay = Overlay::Palette;
        session.show_toast("Undone", false);
        session.pane = Pane::Conversation;
        motion.observe(&session, (false, true), now);
        assert_eq!(motion.overlay(now), (1.0, 0.0));
        let status = motion.status(&session, now);
        assert_eq!(status.notice.unwrap().1, 1.0);
        assert_eq!(status.hints, 0.0);
        assert_eq!(motion.pane_entry(Pane::Conversation, now).opacity, 1.0);
        assert_eq!(motion.focus_edge(Pane::Conversation, now), 1.0);
        session.overlay = Overlay::None;
        session.clear_toast();
        motion.observe(&session, (false, true), now);
        assert!(motion.closing_overlay(now).is_none());
        assert!(motion.status(&session, now).leaving.is_none());
        assert!(!motion.tick(now));
    }
}
